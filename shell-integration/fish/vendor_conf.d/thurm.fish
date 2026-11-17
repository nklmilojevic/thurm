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
    # Clipboard tools that reach the app's clipboard: first on PATH at the first prompt, once
    # config.fish (which runs after this file) has made its changes; off it when that set a
    # display.
    if set -q THURM_CLIP_SHIMS
        function __thurm_clip_shims --on-event fish_prompt
            functions -e __thurm_clip_shims
            # Compared as text: the path may hold characters a pattern would read as a glob.
            set -l rest
            for p in $PATH
                test "$p" = "$THURM_CLIP_SHIMS"; or set -a rest $p
            end
            if test -z "$DISPLAY$WAYLAND_DISPLAY"
                set -gx PATH $THURM_CLIP_SHIMS $rest
            else if set -q rest[1]
                # With a display and nothing but the shims on PATH, PATH stays: empty, it
                # finds nothing.
                set -gx PATH $rest
            end
        end
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
        # The command line (its first), which names the program it starts: an alias's too.
        set -l line (string split -m1 \n -- $argv[1])[1]
        set line (string replace -a '\\' '\\\\' -- $line | string replace -a ';' '\\x3b' | string replace -ra '[\x00-\x1f]' ' ')
        printf '\e]633;E;%s\a' "$line"
        printf '\e]133;C\a'
    end

    function __thurm_postexec --on-event fish_postexec
        printf '\e]133;D;%s\a' $status
    end
end
