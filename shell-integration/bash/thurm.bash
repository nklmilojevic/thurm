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
  # --rcfile skips the system-wide bashrc too (Debian: bash-completion, command-not-found).
  [[ -r /etc/bash.bashrc ]] && builtin source /etc/bash.bashrc
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
    # The prompt commands after this one still see the command's status.
    return "$_thurm_ret"
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
    # The DEBUG trap also fires for PROMPT_COMMAND itself: not a command (an empty Enter
    # would otherwise report one, with the previous exit status).
    [[ "${BASH_COMMAND-}" == _thurm_prompt_start* ]] && return
    if [[ -z "$_thurm_executing" ]]; then
      _thurm_executing=1
      builtin printf '\e]133;C\a'
    fi
  }

  if [[ -n "${bash_preexec_imported-}${__bp_imported-}" ]]; then
    # bash-preexec (atuin, starship...) owns the DEBUG trap: take part in it instead.
    # Its dispatcher has run by the time a precmd hook does: the command's status is the
    # one it saved.
    _thurm_bp_prompt_start() {
      _thurm_ret=${__bp_last_ret_value-$?}
      _thurm_in_prompt=1
    }
    precmd_functions=(_thurm_bp_prompt_start "${precmd_functions[@]}" _thurm_prompt_end)
    preexec_functions+=(_thurm_preexec)
  else
    if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
      # bash 5.1+ runs every element of a PROMPT_COMMAND array (systemd's OSC 3008 profile
      # script appends one): the prompt ends after the last, or the DEBUG trap takes the
      # later ones for the user's command.
      PROMPT_COMMAND[0]="_thurm_prompt_start${PROMPT_COMMAND[0]:+;${PROMPT_COMMAND[0]}}"
      PROMPT_COMMAND+=(_thurm_prompt_end)
    else
      PROMPT_COMMAND="_thurm_prompt_start${PROMPT_COMMAND:+;$PROMPT_COMMAND};_thurm_prompt_end"
    fi
    # Keep a DEBUG trap the user's config set, running it first so it still sees the
    # previous command's $?.
    _thurm_prev_debug=$(builtin trap -p DEBUG)
    _thurm_prev_debug=${_thurm_prev_debug#"trap -- '"}
    _thurm_prev_debug=${_thurm_prev_debug%"' DEBUG"}
    _thurm_prev_debug=${_thurm_prev_debug//"'\\''"/"'"}
    if [[ -n "$_thurm_prev_debug" ]]; then
      _thurm_debug() {
        builtin eval "$_thurm_prev_debug"
        _thurm_preexec
      }
      builtin trap '_thurm_debug' DEBUG
    else
      builtin trap '_thurm_preexec' DEBUG
    fi
  fi
fi
