pub fn print_completions(shell: &str) {
    match shell {
        "bash" => print!("{}", BASH),
        "zsh" => print!("{}", ZSH),
        "fish" => print!("{}", FISH),
        "nu" | "nushell" => print!("{}", NU),
        _ => {
            eprintln!("error: unsupported shell '{}' (bash, zsh, fish, nu)", shell);
            std::process::exit(1);
        }
    }
}

const BASH: &str = r#"_rift_completions() {
  local cur prev words cword
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"

  local commands="--new attach new run send print write tail detach list get set unset clear completions kill history wait version help rename rn logs lg last la print-env pe"

  if [[ $COMP_CWORD -eq 1 ]]; then
    local sessions=$(rift list --short 2>/dev/null | tr '\n' ' ')
    COMPREPLY=($(compgen -W "$commands $sessions" -- "$cur") $(compgen -c -- "$cur"))
    return 0
  fi

  case "$prev" in
    --new)
      COMPREPLY=($(compgen -c -- "$cur"))
      ;;
    attach|a|new|n|run|r|send|s|print|p|write|wr|tail|t|kill|k|history|hi|detach|d|wait|w|rename|rn|logs|lg|get|g|set|unset|un|clear|cl|print-env|pe)
      local sessions=$(rift list --short 2>/dev/null | tr '\n' ' ')
      COMPREPLY=($(compgen -W "$sessions" -- "$cur"))
      ;;
    completions)
      COMPREPLY=($(compgen -W "bash zsh fish nu" -- "$cur"))
      ;;
    list|ls|l)
      COMPREPLY=($(compgen -W "--short --verbose --where" -- "$cur"))
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
    '--new[Run command in a new automatically named session]:command:_command_names' \
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
        'get:Get session labels'
        'set:Set session labels'
        'unset:Remove session labels'
        'clear:Clear session labels'
        'completions:Print shell completion script'
        'kill:Kill a session'
        'history:Print session output'
        'logs:Tail -f the session log file'
        'last:Attach to the most recently attached session'
        'print-env:Print the leader client'\''s tracked env vars'
        'wait:Wait for sessions to complete'
        'version:Print version'
        'help:Print help'
      )
      _describe 'rift command' commands
      _command_names
      ;;
    args)
      case $words[2] in
        attach|a|new|n|kill|k|run|r|send|s|print|p|write|wr|tail|t|detach|d|history|hi|wait|w|rename|rn|logs|lg|get|g|set|unset|un|clear|cl|print-env|pe)
          _rift_sessions
          ;;
        completions)
          _values 'shell' 'bash' 'zsh' 'fish' 'nu'
          ;;
        list|ls|l)
          _values 'options' '--short' '--verbose' '--where'
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

complete -c rift -l new -r -a '(__fish_complete_command)' -d 'Run command in a new automatically named session'

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
complete -c rift -n "__fish_is_nth_token 1" -a 'g get' -d 'Get session labels'
complete -c rift -n "__fish_is_nth_token 1" -a 'set' -d 'Set session labels'
complete -c rift -n "__fish_is_nth_token 1" -a 'un unset' -d 'Remove session labels'
complete -c rift -n "__fish_is_nth_token 1" -a 'cl clear' -d 'Clear session labels'
complete -c rift -n "__fish_is_nth_token 1" -a 'c completions' -d 'Print shell completion script'
complete -c rift -n "__fish_is_nth_token 1" -a 'k kill' -d 'Kill a session'
complete -c rift -n "__fish_is_nth_token 1" -a 'hi history' -d 'Print session output'
complete -c rift -n "__fish_is_nth_token 1" -a 'lg logs' -d 'Tail -f the session log file'
complete -c rift -n "__fish_is_nth_token 1" -a 'la last' -d 'Attach to the most recently attached session'
complete -c rift -n "__fish_is_nth_token 1" -a 'pe print-env' -d 'Print the leader client tracked env vars'
complete -c rift -n "__fish_is_nth_token 1" -a 'w wait' -d 'Wait for sessions to complete'
complete -c rift -n "__fish_is_nth_token 1" -a 'v version' -d 'Print version'
complete -c rift -s V -l version -d 'Print version'
complete -c rift -n "__fish_is_nth_token 1" -a 'h help' -d 'Print help'
complete -c rift -s h -d 'Print help'
complete -c rift -n "__fish_is_nth_token 1" -a '(__fish_complete_command)' -d 'Command'

complete -c rift -n "__fish_is_nth_token 2; and __fish_seen_subcommand_from attach a new n run r send s print p write wr detach d history hi rename rn logs lg get g set unset un clear cl print-env pe" -a '(rift list --short 2>/dev/null)' -d 'Session name'
complete -c rift -n "__fish_seen_subcommand_from tail t kill k wait w; and not __fish_seen_subcommand_from --help -h" -a '(rift list --short 2>/dev/null)' -d 'Session name'

complete -c rift -n "__fish_is_nth_token 2; and __fish_seen_subcommand_from completions c" -a 'bash zsh fish nu' -d Shell

complete -c rift -n "__fish_seen_subcommand_from list ls l" -l short -s s -d 'Short output'
complete -c rift -n "__fish_seen_subcommand_from list ls l" -l verbose -s v -d 'Verbose output (uptime, log path)'
complete -c rift -n "__fish_seen_subcommand_from list ls l" -l where -r -d 'Filter by label (key=value)'
complete -c rift -n "__fish_seen_subcommand_from history hi" -l vt -d 'VT escape sequence format'
complete -c rift -n "__fish_seen_subcommand_from history hi" -l html -d 'HTML format'
complete -c rift -n "__fish_seen_subcommand_from attach a" -s d -l detached -d 'Create without attaching'
complete -c rift -n "__fish_seen_subcommand_from attach a new n" -l labels -r -d 'Set labels at creation (k=v ...)'
complete -c rift -n "__fish_seen_subcommand_from print-env pe" -s s -l shell -d 'Emit export/unset for eval'
complete -c rift -n "__fish_seen_subcommand_from run r" -s d -l detached -d 'Run detached (background)'
complete -c rift -n "__fish_seen_subcommand_from run r" -l fish -d 'Use fish shell completion detection'
complete -c rift -n "__fish_seen_subcommand_from kill k" -s f -l force -d 'Force kill (SIGKILL)'
"#;

const NU: &str = r#"def "nu-complete rift sessions" [] {
  rift list --short | lines
}

def "nu-complete rift shells" [] {
  [bash zsh fish nu]
}

export extern rift [
  --new: string
  ...rest: string
]

export extern "rift attach" [
  name: string@"nu-complete rift sessions"
  -d
  --labels: string
  ...rest: string
]

export extern "rift new" [
  name: string
  --labels: string
  ...rest: string
]

export extern "rift run" [
  name: string@"nu-complete rift sessions"
  -d
  --fish
  ...rest: string
]

export extern "rift send" [
  name: string@"nu-complete rift sessions"
  ...text: string
]

export extern "rift print" [
  name: string@"nu-complete rift sessions"
  ...text: string
]

export extern "rift write" [
  name: string@"nu-complete rift sessions"
  path: path
]

export extern "rift kill" [
  ...names: string@"nu-complete rift sessions"
  --force(-f)
]

export extern "rift detach" [name?: string@"nu-complete rift sessions"]
export extern "rift list" [--short(-s), --verbose(-v), --where: string]
export extern "rift get" [name: string@"nu-complete rift sessions", key?: string]
export extern "rift set" [name: string@"nu-complete rift sessions", ...labels: string]
export extern "rift unset" [name: string@"nu-complete rift sessions", ...keys: string]
export extern "rift clear" [name: string@"nu-complete rift sessions"]
export extern "rift history" [name: string@"nu-complete rift sessions", --vt, --html]
export extern "rift wait" [...names: string@"nu-complete rift sessions"]
export extern "rift tail" [...names: string@"nu-complete rift sessions"]
export extern "rift logs" [name: string@"nu-complete rift sessions", ...rest: string]
export extern "rift rename" [name?: string@"nu-complete rift sessions", new_name: string]
export extern "rift last" []
export extern "rift print-env" [name?: string@"nu-complete rift sessions", key?: string, --shell(-s)]
export extern "rift version" []
export extern "rift completions" [shell: string@"nu-complete rift shells"]
export extern "rift help" []
"#;
