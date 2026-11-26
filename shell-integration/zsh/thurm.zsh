# Thurm zsh integration: OSC 133 prompt marks, OSC 7 working directory.
[[ -n "${_THURM_ZSH_LOADED-}" ]] && return
typeset -g _THURM_ZSH_LOADED=1
typeset -g _thurm_executing=""
# Proves to Thurm that the $PATH report comes from this shell, not from program output.
# Kept out of the environment of the programs the shell runs.
typeset -g _thurm_token="${THURM_SHELL_TOKEN-}"
builtin unset THURM_SHELL_TOKEN

# Make the bundled `thurm` CLI reachable (appended, so an installed one wins).
if [[ -n "${THURM_BIN_DIR-}" && ":$PATH:" != *":$THURM_BIN_DIR:"* ]]; then
  export PATH="$PATH:$THURM_BIN_DIR"
fi

_thurm_urlencode() {
  local LC_ALL=C s="$1" out="" c i
  for (( i = 1; i <= ${#s}; i++ )); do
    c="${s[i]}"
    case "$c" in
      [a-zA-Z0-9/._~-]) out+="$c" ;;
      *) out+=$(printf '%%%02X' "'$c") ;;
    esac
  done
  print -rn -- "$out"
}

_thurm_precmd() {
  local ret=$?
  if [[ -n "$_thurm_executing" ]]; then
    builtin printf '\e]133;D;%s\a' "$ret"
  fi
  _thurm_executing=""
  builtin printf '\e]7;file://%s%s\a' "${HOST}" "$(_thurm_urlencode "$PWD")"
  # $PATH for Thurm's tab completion (only when it changed).
  if [[ -n "$_thurm_token" && "$PATH" != "${_thurm_last_path-}" ]]; then
    builtin printf '\e]633;P;ThurmPath=%s:%s\a' "$_thurm_token" "${PATH//;/\\x3b}"
    _thurm_last_path=$PATH
  fi
  builtin printf '\e]133;A\a'
  # Mark the end of the prompt (prompt themes may rebuild PS1 every time).
  if [[ "$PS1" != *'133;B'* ]]; then
    PS1="${PS1}%{"$'\e]133;B\a'"%}"
  fi
}

_thurm_preexec() {
  # The command line (its first), which names the program it starts: an alias's too.
  local line=${1%%$'\n'*}
  line=${line//\\/\\\\}
  line=${line//;/\\x3b}
  line=${line//[[:cntrl:]]/ }
  builtin printf '\e]633;E;%s\a' "$line"
  builtin printf '\e]133;C\a'
  _thurm_executing=1
}

# Run after every other precmd hook so PS1 changes by themes are already applied.
_thurm_precmd_last() {
  precmd_functions=(${precmd_functions:#_thurm_precmd} _thurm_precmd)
  precmd_functions=(${precmd_functions:#_thurm_precmd_last})
  # Clipboard tools that reach the app's clipboard: first on PATH once .zprofile and .zshrc
  # (which run after this file) have made their changes; off it when those set a display.
  if [[ -n "${THURM_CLIP_SHIMS-}" ]]; then
    # The directory as text: quoted for a pattern, read as one even under GLOB_SUBST.
    # Quoted, so empty entries (the current directory) stay where they were.
    local -a _thurm_rest=("${(@)path:#${~${(b)THURM_CLIP_SHIMS}}}")
    if [[ -z "${DISPLAY-}${WAYLAND_DISPLAY-}" ]]; then
      path=("$THURM_CLIP_SHIMS" "${(@)_thurm_rest}")
    elif (( ${#_thurm_rest} )); then
      # With a display and nothing but the shims on PATH, PATH stays: empty, it finds nothing.
      path=("${(@)_thurm_rest}")
    fi
    export PATH
  fi
  _thurm_precmd
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _thurm_precmd_last
add-zsh-hook preexec _thurm_preexec

# One-shot program resume after a restart (Thurm session restore). It runs as the first
# prompt's command line once zle starts: after every precmd hook .zshrc added (direnv, mise,
# whatever their names), as an ordinary interactive job (Ctrl-Z, fg), and outside any hook,
# so a failure neither skips the prompt marks nor ends the shell. The ticket is a bearer
# secret: never expanded where xtrace (set by a startup file) would print it.
if (( ${#${_thurm_resume_ticket-}} && ${#${_thurm_resume_exec-}} )); then
  # The command line holds only this function's name: zsh redraws an accepted line, and
  # preexec hooks (Thurm's command-line report among them) see it, so the single-use ticket
  # and the helper's path stay in these private variables until it runs.
  _thurm_resume_run() {
    builtin setopt LOCAL_OPTIONS NO_XTRACE
    builtin unfunction _thurm_resume_run
    local exec="${_thurm_resume_exec-}" ticket="${_thurm_resume_ticket-}"
    _thurm_resume_ticket=""
    _thurm_resume_exec=""
    [[ -n "$exec" && -n "$ticket" ]] || return 0
    command "$exec" --exec-resume "$ticket" || :
  }
  _thurm_resume_line_init() {
    add-zle-hook-widget -d line-init _thurm_resume_line_init
    (( ${#${_thurm_resume_ticket-}} )) || return 0
    # Typed ahead of this prompt: that line is the user's, and the resume is dropped (the
    # daemon refuses the ticket after typing anyway).
    if (( ${PENDING:-0} || ${KEYS_QUEUED_COUNT:-0} )); then
      _thurm_resume_ticket=""
      _thurm_resume_exec=""
      builtin unfunction _thurm_resume_run 2>/dev/null
      return 0
    fi
    # Never the program's arguments, the helper or the ticket: only a fixed name. The leading
    # space and the history hook keep it out of history.
    BUFFER=" _thurm_resume_run"
    add-zsh-hook zshaddhistory _thurm_resume_nohist
    zle .accept-line
  }
  _thurm_resume_nohist() {
    add-zsh-hook -d zshaddhistory _thurm_resume_nohist
    return 1
  }
  # Armed at the first prompt, once .zshrc defined its own zle hooks (kept and run too).
  _thurm_resume_arm() {
    add-zsh-hook -d precmd _thurm_resume_arm
    if [[ -o zle ]]; then
      autoload -Uz add-zle-hook-widget
      zle -N _thurm_resume_line_init
      add-zle-hook-widget line-init _thurm_resume_line_init
      return 0
    fi
    # No line editor (.zshrc turned ZLE off): no command line to run it as, so it runs from
    # this hook, after the startup files but before precmd hooks they added. It is a command,
    # not the prompt, while it runs (the prompt marks come again after it); the daemon
    # refuses the ticket if anything was typed first.
    builtin printf '\e]133;C\a'
    _thurm_executing=1
    _thurm_resume_run
    _thurm_precmd
  }
  add-zsh-hook precmd _thurm_resume_arm
fi
