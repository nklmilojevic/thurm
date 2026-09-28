# Thurm shell integration bootstrap.
#
# Thurm points ZDOTDIR here so this file runs first. Restore the user's ZDOTDIR, source
# their .zshenv, then load the integration for interactive shells. zsh reads .zprofile,
# .zshrc and .zlogin from the restored ZDOTDIR afterwards, so user config is untouched.
if [[ -n "${THURM_ORIG_ZDOTDIR+x}" ]]; then
  ZDOTDIR="$THURM_ORIG_ZDOTDIR"
  unset THURM_ORIG_ZDOTDIR
else
  unset ZDOTDIR
fi

{
  typeset _thurm_zshenv="${ZDOTDIR-$HOME}/.zshenv"
  [[ -r "$_thurm_zshenv" ]] && builtin source -- "$_thurm_zshenv"
} always {
  if [[ -o interactive && -n "${THURM_SHELL_INTEGRATION-}" ]]; then
    builtin source -- "$THURM_SHELL_INTEGRATION/zsh/thurm.zsh"
  fi
  builtin unset _thurm_zshenv
}
