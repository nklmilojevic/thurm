# Thurm fish integration: OSC 133 prompt marks, OSC 7 working directory.
if set -q THURM_ORIG_XDG_DATA_DIRS
    if test -n "$THURM_ORIG_XDG_DATA_DIRS"
        set -gx XDG_DATA_DIRS $THURM_ORIG_XDG_DATA_DIRS
    else
        set -e XDG_DATA_DIRS
    end
    set -e THURM_ORIG_XDG_DATA_DIRS
end

# One-shot program resume after a restart (Thurm session restore): fish never gets the
# ticket in its environment (Thurm hands it over in the `-C` command, which fish runs after
# config.fish), so nothing config.fish starts can inherit it.
set -e THURM_RESUME_TICKET
test -z "$THURM_RESUME_EXEC"; and set -e THURM_RESUME_EXEC

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
        # A restore's resume runs the prompt handlers ahead of the program: not a prompt yet.
        set -q __thurm_resuming; and return
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

# Run by the `-C` command Thurm starts a restored fish with, which sets the ticket and sources
# this file again after config.fish. It runs as a top-level command there: fish hands such a
# job the terminal (its process group in the foreground, the terminal's own modes) in every
# job-control mode, which it never does for a job an event handler starts, and it comes before
# fish's first prompt queries the terminal, whose answers would otherwise be read as input.
# The prompt handlers config.fish registered (direnv's, mise's) run first, so the program is
# looked up on the PATH they make; they run again at the first prompt. Thurm's own prompt mark
# waits for that prompt: the program's output is not a prompt to clear on resize.
if status is-interactive; and set -q __thurm_resume_ticket; and test -n "$THURM_RESUME_EXEC"
    set -l ticket $__thurm_resume_ticket
    set -l exec $THURM_RESUME_EXEC
    set -e __thurm_resume_ticket
    set -e THURM_RESUME_EXEC
    set -g __thurm_resuming 1
    emit fish_prompt
    set -e __thurm_resuming
    # Its own process group, as for a command typed at a prompt, so Ctrl-Z stops it (fish
    # treats a `-C` command as non-interactive in its default job-control mode).
    set -l jc none
    status is-interactive-job-control; and set jc interactive
    status is-full-job-control; and set jc full
    status job-control full
    printf '\e]133;C\a'
    # Its own evaluation: a job ended by Ctrl-C or Ctrl-\ cancels the rest of the evaluation
    # it runs in, which must not be this one (the mark and the job-control mode after it).
    # A fixed string, expanding the two variables only inside.
    builtin eval 'command $exec --exec-resume $ticket'
    printf '\e]133;D;%s\a' $status
    status job-control $jc
end
