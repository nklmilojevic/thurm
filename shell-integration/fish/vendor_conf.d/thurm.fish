# Thurm fish integration: OSC 133 prompt marks, OSC 7 working directory.
if set -q THURM_ORIG_XDG_DATA_DIRS
    if test -n "$THURM_ORIG_XDG_DATA_DIRS"
        set -gx XDG_DATA_DIRS $THURM_ORIG_XDG_DATA_DIRS
    else
        set -e XDG_DATA_DIRS
    end
    set -e THURM_ORIG_XDG_DATA_DIRS
end

if status is-interactive; and not set -q __thurm_fish_loaded
    set -g __thurm_fish_loaded 1
    # Proves to Thurm that the $PATH report comes from this shell, not from program output.
    # Kept out of the environment of the programs the shell runs.
    set -g __thurm_token "$THURM_SHELL_TOKEN"
    set -e THURM_SHELL_TOKEN

    # Make the bundled `thurm` CLI reachable (appended, so an installed one wins).
    if set -q THURM_BIN_DIR; and not contains -- $THURM_BIN_DIR $PATH
        set -gx PATH $PATH $THURM_BIN_DIR
    end

    function __thurm_prompt --on-event fish_prompt
        printf '\e]7;file://%s%s\a' $hostname (string escape --style=url -- $PWD)
        # $PATH for Thurm's tab completion (only when it changed).
        set -l path (string join : -- $PATH)
        if test -n "$__thurm_token"; and test "$path" != "$__thurm_last_path"
            printf '\e]633;P;ThurmPath=%s:%s\a' $__thurm_token (string replace -a ';' '\\x3b' -- $path)
            set -g __thurm_last_path $path
        end
        # fish redraws its prompt on resize: Thurm clears the old one first (no copies).
        printf '\e]133;A;redraw=1\a'
    end

    function __thurm_preexec --on-event fish_preexec
        printf '\e]133;C\a'
    end

    function __thurm_postexec --on-event fish_postexec
        printf '\e]133;D;%s\a' $status
    end
end
