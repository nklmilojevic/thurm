# Thurm bash integration: OSC 133 prompt marks, OSC 7 working directory.
#
# Bash is started as `bash --rcfile <this file> -i`. Emulate the usual startup files first.
if [[ -n "${THURM_BASH_LOGIN-}" ]]; then
  builtin unset THURM_BASH_LOGIN
  [[ -r /etc/profile ]] && builtin source /etc/profile
  for _thurm_f in ~/.bash_profile ~/.bash_login ~/.profile; do
    if [[ -r "$_thurm_f" ]]; then builtin source "$_thurm_f"; break; fi
  done
  builtin unset _thurm_f
else
  [[ -r ~/.bashrc ]] && builtin source ~/.bashrc
fi

if [[ -z "${_THURM_BASH_LOADED-}" ]]; then
  _THURM_BASH_LOADED=1
  _thurm_executing=""
  _thurm_in_prompt=""
  _thurm_ret=0
  # Proves to Thurm that the $PATH report comes from this shell, not from program output.
  # Kept out of the environment of the programs the shell runs.
  _thurm_token=${THURM_SHELL_TOKEN-}
  builtin unset THURM_SHELL_TOKEN

  # Make the bundled `thurm` CLI reachable (appended, so an installed one wins).
  if [[ -n "${THURM_BIN_DIR-}" && ":$PATH:" != *":$THURM_BIN_DIR:"* ]]; then
    export PATH="$PATH:$THURM_BIN_DIR"
  fi

  _thurm_prompt_start() {
    _thurm_ret=$?
    _thurm_in_prompt=1
  }

  _thurm_prompt_end() {
    if [[ -n "$_thurm_executing" ]]; then
      builtin printf '\e]133;D;%s\a' "$_thurm_ret"
    fi
    _thurm_executing=""
    builtin printf '\e]7;file://%s%s\a' "$HOSTNAME" "${PWD// /%20}"
    # $PATH for Thurm's tab completion (only when it changed).
    if [[ -n "$_thurm_token" && "$PATH" != "${_thurm_last_path-}" ]]; then
      builtin printf '\e]633;P;ThurmPath=%s:%s\a' "$_thurm_token" "${PATH//;/\\x3b}"
      _thurm_last_path=$PATH
    fi
    builtin printf '\e]133;A\a'
    if [[ "$PS1" != *'133;B'* ]]; then
      PS1="${PS1}"'\[\e]133;B\a\]'
    fi
    _thurm_in_prompt=""
  }

  _thurm_preexec() {
    [[ -n "$_thurm_in_prompt" || -n "${COMP_LINE-}" ]] && return
    if [[ -z "$_thurm_executing" ]]; then
      _thurm_executing=1
      builtin printf '\e]133;C\a'
    fi
  }

  PROMPT_COMMAND="_thurm_prompt_start${PROMPT_COMMAND:+;$PROMPT_COMMAND};_thurm_prompt_end"
  trap '_thurm_preexec' DEBUG
fi
