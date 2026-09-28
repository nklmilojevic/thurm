# Thurm zsh integration: OSC 133 prompt marks, OSC 7 working directory.
[[ -n "${_THURM_ZSH_LOADED-}" ]] && return
typeset -g _THURM_ZSH_LOADED=1
typeset -g _thurm_executing=""

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
  if [[ "$PATH" != "${_thurm_last_path-}" ]]; then
    builtin printf '\e]633;P;ThurmPath=%s\a' "${PATH//;/\\x3b}"
    _thurm_last_path=$PATH
  fi
  builtin printf '\e]133;A\a'
  # Mark the end of the prompt (prompt themes may rebuild PS1 every time).
  if [[ "$PS1" != *'133;B'* ]]; then
    PS1="${PS1}%{"$'\e]133;B\a'"%}"
  fi
}

_thurm_preexec() {
  builtin printf '\e]133;C\a'
  _thurm_executing=1
}

# Run after every other precmd hook so PS1 changes by themes are already applied.
_thurm_precmd_last() {
  precmd_functions=(${precmd_functions:#_thurm_precmd} _thurm_precmd)
  precmd_functions=(${precmd_functions:#_thurm_precmd_last})
  _thurm_precmd
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _thurm_precmd_last
add-zsh-hook preexec _thurm_preexec
