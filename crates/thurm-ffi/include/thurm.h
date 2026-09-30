/*
 * thurm.h — C interface between the Thurm macOS app (Swift) and the Rust core.
 *
 * The Rust side (libthurm_ffi.a) connects to the session daemon `thurmd`, which owns
 * every PTY and terminal state. The app is a thin client: it forwards input, receives
 * screen frames and renders them with Metal + CoreText.
 *
 * Threading: every function may be called from any thread. Callbacks are invoked on a
 * Rust background thread; hop to the main queue before touching AppKit.
 *
 * Strings returned as `char *` are heap allocated and must be released with
 * thurm_string_free(). Strings returned as `const char *` are borrowed.
 */
#ifndef THURM_H
#define THURM_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct thurm_client thurm_client;
typedef uint64_t thurm_pane_id;

/* A daemon event, JSON encoded. See docs/PROTOCOL.md for the shapes. Examples:
 *   {"PaneInfo":{"id":1,"title":"zsh",...}}
 *   {"PaneExited":{"pane":1,"code":0}}
 *   {"Notify":{"pane":1,"title":"Claude Code","body":"needs your input"}}
 *   {"Ui":{"Split":{"target":1,"pane":4,"dir":"right"}}}
 *   "ConfigReloaded"                     (unit variants are bare strings)
 *   "Disconnected"                       (connection to the daemon was lost)
 *   {"ClipboardStore":{"pane":1,"text":"...","user":true}}   (copy on select: the user's own
 *                                        selection; without "user" it is a program's OSC 52)
 */
typedef void (*thurm_event_cb)(void *ctx, const char *json);

/* New screen contents are available for `pane`: call thurm_grid_lock() on the next display
 * refresh. Coalesced: called at most once per frame the daemon sends. */
typedef void (*thurm_frame_cb)(void *ctx, thurm_pane_id pane);

/* Connect to the daemon, spawning `daemon_path` (the thurmd executable, e.g. inside the app
 * bundle) if no daemon is running. `daemon_path` may be NULL to never spawn.
 * `client_name` identifies the client in logs. Returns NULL on failure and, when `err` is
 * non-NULL, stores an error string to free with thurm_string_free(). */
thurm_client *thurm_connect(const char *daemon_path, const char *client_name,
                            thurm_event_cb on_event, thurm_frame_cb on_frame, void *ctx,
                            char **err);

/* Like thurm_connect, to the daemon listening on `socket`: a remote host's daemon through the
 * tunnel's local end (see thurm_remotes_start). Each connection has its own panes: pane ids
 * are only unique per connection. `ctx` comes back with every callback, so it can say which
 * connection an event or frame belongs to. */
thurm_client *thurm_connect_socket(const char *socket, const char *daemon_path,
                                   const char *client_name, thurm_event_cb on_event,
                                   thurm_frame_cb on_frame, void *ctx, char **err);

/* Close the connection. Panes keep running inside the daemon. */
void thurm_disconnect(thurm_client *client);

/* Write `data` to a new file (0600) in the daemon's runtime directory, on the daemon's machine,
 * and return its path (NULL with `err` set on failure). An image pasted into a remote pane goes
 * over like this, and the program gets a path it can read. Free the result. */
char *thurm_write_temp_file(thurm_client *client, const char *name, const uint8_t *data,
                            size_t len, char **err);

/* Re-read this Mac's config into the connection's terminal copies. The local daemon's
 * "ConfigReloaded" does this for its own connection; call it for remote connections. */
void thurm_reload_engine(thurm_client *client);

void thurm_string_free(char *s);

/* ---------------------------------------------------------------------------------------
 * Generic request: `json` is a serde-JSON encoded `Request` (externally tagged), e.g.
 *   {"CreatePane":{"command":null,"cwd":"/tmp","env":[],"size":{"cols":80,"rows":24,
 *     "cell_width":8,"cell_height":16},"agent_preset":null,"inherit_cwd_from":null,"hold":false}}
 *   {"ClosePane":{"pane":3}}
 *   "ListPanes"
 *   {"SetLayout":{"json":"{...layout json...}"}}
 *   "GetLayout"
 *   {"Selection":{"pane":1,"op":{"Start":{"col":0,"row":0,"right_half":false,"kind":"Simple"}}}}
 *   {"Search":{"pane":1,"query":"error","direction":"Backward"}}
 *   {"Scroll":{"pane":1,"scroll":"PageUp"}}   {"Scroll":{"pane":1,"scroll":{"Lines":3}}}
 * Blocks until the daemon answers. Returns a JSON `Response` such as {"PaneCreated":{"pane":5}},
 * "Ok", {"Text":"..."}, {"Layout":"..."|null}, or {"error":"message"}. Free the result.
 * ------------------------------------------------------------------------------------- */
char *thurm_request(thurm_client *client, const char *json);

/* Same as thurm_request but does not wait for the answer. */
void thurm_send(thurm_client *client, const char *json);

/* ---------------------------------------------------------------------------------------
 * Hot paths (asynchronous, no JSON).
 * ------------------------------------------------------------------------------------- */

void thurm_subscribe(thurm_client *client, thurm_pane_id pane);
void thurm_unsubscribe(thurm_client *client, thurm_pane_id pane);

/* Raw bytes to the PTY. */
void thurm_input(thurm_client *client, thurm_pane_id pane, const uint8_t *data, size_t len);

/* User paste (bracketed paste is applied when the program enabled it). UTF-8. */
void thurm_paste(thurm_client *client, thurm_pane_id pane, const char *text);

void thurm_resize(thurm_client *client, thurm_pane_id pane, uint16_t cols, uint16_t rows,
                  uint16_t cell_width_px, uint16_t cell_height_px);

void thurm_focus(thurm_client *client, thurm_pane_id pane, bool focused);

/* Modifier bits (kitty keyboard protocol encoding). */
enum {
    THURM_MOD_SHIFT = 1,
    THURM_MOD_ALT = 2,
    THURM_MOD_CTRL = 4,
    THURM_MOD_SUPER = 8,
    THURM_MOD_HYPER = 16,
    THURM_MOD_META = 32,
    THURM_MOD_CAPS_LOCK = 64,
    THURM_MOD_NUM_LOCK = 128,
};

enum { THURM_KEY_PRESS = 0, THURM_KEY_REPEAT = 1, THURM_KEY_RELEASE = 2 };

/* Named (non text) keys. */
enum {
    THURM_KEY_ESCAPE = 1,
    THURM_KEY_ENTER,
    THURM_KEY_TAB,
    THURM_KEY_BACKSPACE,
    THURM_KEY_INSERT,
    THURM_KEY_DELETE,
    THURM_KEY_LEFT,
    THURM_KEY_RIGHT,
    THURM_KEY_UP,
    THURM_KEY_DOWN,
    THURM_KEY_PAGE_UP,
    THURM_KEY_PAGE_DOWN,
    THURM_KEY_HOME,
    THURM_KEY_END,
    THURM_KEY_CAPS_LOCK,
    THURM_KEY_SCROLL_LOCK,
    THURM_KEY_NUM_LOCK,
    THURM_KEY_PRINT_SCREEN,
    THURM_KEY_PAUSE,
    THURM_KEY_MENU,
    THURM_KEY_KP_0, /* KP_0 .. KP_9 are consecutive */
    THURM_KEY_KP_1,
    THURM_KEY_KP_2,
    THURM_KEY_KP_3,
    THURM_KEY_KP_4,
    THURM_KEY_KP_5,
    THURM_KEY_KP_6,
    THURM_KEY_KP_7,
    THURM_KEY_KP_8,
    THURM_KEY_KP_9,
    THURM_KEY_KP_DECIMAL,
    THURM_KEY_KP_DIVIDE,
    THURM_KEY_KP_MULTIPLY,
    THURM_KEY_KP_SUBTRACT,
    THURM_KEY_KP_ADD,
    THURM_KEY_KP_ENTER,
    THURM_KEY_KP_EQUAL,
    THURM_KEY_LEFT_SHIFT,
    THURM_KEY_LEFT_CONTROL,
    THURM_KEY_LEFT_ALT,
    THURM_KEY_LEFT_SUPER,
    THURM_KEY_RIGHT_SHIFT,
    THURM_KEY_RIGHT_CONTROL,
    THURM_KEY_RIGHT_ALT,
    THURM_KEY_RIGHT_SUPER,
    THURM_KEY_VOLUME_DOWN,
    THURM_KEY_VOLUME_UP,
    THURM_KEY_VOLUME_MUTE,
    THURM_KEY_F1 = 100, /* F1 .. F35 are 100 .. 134 */
};

typedef struct {
    /* 0 = text key (`code` is the unshifted Unicode scalar, e.g. 'a' for Shift+A),
     * 1 = named key (`code` is a THURM_KEY_* value). */
    uint32_t kind;
    uint32_t code;
    uint8_t mods;
    uint8_t action;
    /* Text produced by the key (after Option/IME), UTF-8, may be NULL. */
    const char *text;
    /* Shifted character (e.g. 'A' or '!'), 0 when none. */
    uint32_t shifted;
    /* The key in the US layout (for non-Latin layouts), 0 when none. */
    uint32_t base_layout;
} thurm_key_event;

/* Encoded by the daemon according to the pane's current mode (legacy / kitty protocol). */
void thurm_key(thurm_client *client, thurm_pane_id pane, const thurm_key_event *event);

enum { THURM_MOUSE_PRESS = 0, THURM_MOUSE_RELEASE = 1, THURM_MOUSE_MOVE = 2 };
enum {
    THURM_BUTTON_LEFT = 0,
    THURM_BUTTON_MIDDLE = 1,
    THURM_BUTTON_RIGHT = 2,
    THURM_BUTTON_BACK = 3,
    THURM_BUTTON_FORWARD = 4,
    THURM_BUTTON_NONE = 5,
};

typedef struct {
    uint8_t kind;
    uint8_t button;
    uint8_t mods;
    uint8_t clicks; /* 1, 2 (word) or 3 (line) */
    uint16_t col;   /* viewport cell */
    uint16_t row;
    bool right_half; /* pointer on the right half of the cell */
    uint32_t x;      /* pixels inside the pane */
    uint32_t y;
} thurm_mouse_event;

/* Mouse reporting or selection handling (Shift forces selection while an app grabs the mouse). */
void thurm_mouse(thurm_client *client, thurm_pane_id pane, const thurm_mouse_event *event);

/* Scroll wheel, in whole lines (positive = towards older output). Reported to mouse-grabbing
 * apps, turned into arrow keys on the alternate screen, or scrolls the scrollback. */
void thurm_wheel(thurm_client *client, thurm_pane_id pane, int32_t lines, uint16_t col,
                 uint16_t row, uint8_t mods);

/* ---------------------------------------------------------------------------------------
 * Screen contents.
 * ------------------------------------------------------------------------------------- */

/* Cell flags. */
enum {
    THURM_CELL_BOLD = 1 << 0,
    THURM_CELL_ITALIC = 1 << 1,
    THURM_CELL_UNDERLINE = 1 << 2,
    THURM_CELL_DOUBLE_UNDERLINE = 1 << 3,
    THURM_CELL_UNDERCURL = 1 << 4,
    THURM_CELL_DOTTED_UNDERLINE = 1 << 5,
    THURM_CELL_DASHED_UNDERLINE = 1 << 6,
    THURM_CELL_STRIKEOUT = 1 << 7,
    THURM_CELL_WIDE = 1 << 8,        /* first half of a double-width char */
    THURM_CELL_WIDE_SPACER = 1 << 9, /* second half: draw nothing */
    THURM_CELL_HIDDEN = 1 << 10,
    THURM_CELL_SELECTED = 1 << 11,
    THURM_CELL_SEARCH_MATCH = 1 << 12,
    THURM_CELL_DEFAULT_BG = 1 << 13, /* background is the default one (may be translucent) */
    THURM_CELL_DIM = 1 << 14,
    THURM_CELL_SEARCH_FOCUS = 1 << 15,
};

#define THURM_NO_COLOR 0xFF000000u

typedef struct {
    uint32_t ch; /* Unicode scalar; 0 or ' ' = blank */
    uint32_t fg; /* 0x00RRGGBB, already resolved (inverse/dim applied) */
    uint32_t bg;
    uint32_t ul; /* underline color, THURM_NO_COLOR = use fg */
    uint16_t flags;
    uint16_t link; /* hyperlink index + 1, 0 = none */
} thurm_cell;

enum {
    THURM_CURSOR_BLOCK = 0,
    THURM_CURSOR_UNDERLINE = 1,
    THURM_CURSOR_BEAM = 2,
    THURM_CURSOR_HOLLOW_BLOCK = 3,
    THURM_CURSOR_HIDDEN = 4,
};

typedef struct {
    uint16_t cols;
    uint16_t rows;
    uint16_t cursor_col;
    uint16_t cursor_row;
    uint8_t cursor_shape;
    bool cursor_blinking;
    bool cursor_wide;
    uint32_t display_offset; /* > 0 while scrolled back */
    uint32_t history_size;
    uint32_t modes; /* TermMode bits, see THURM_MODE_* */
    uint32_t foreground;
    uint32_t background;
    uint32_t cursor_color;
    uint32_t cursor_text_color;
    uint32_t selection_fg;
    uint32_t selection_bg;
    /* Incremented on every applied frame; skip re-rendering when unchanged. */
    uint64_t generation;
    /* Bitmask of rows changed since the previous thurm_grid_lock for this pane
     * (bit i = row i, rows >= 64 fold into bit 63). All ones after a full refresh. */
    uint64_t dirty_rows;
    size_t image_count;
    size_t link_count;
    /* Rows moved down (negative: up) since the previous thurm_grid_lock because the
     * scrollback position changed: row r shows what row r - shift showed. `dirty_rows` is
     * already expressed in the new positions. */
    int32_t shift;
    /* A history line above the viewport is available (thurm_grid_peek). */
    bool has_peek;
} thurm_grid_info;

enum {
    THURM_MODE_SHOW_CURSOR = 1 << 0,
    THURM_MODE_APP_CURSOR = 1 << 1,
    THURM_MODE_MOUSE_REPORT_CLICK = 1 << 3,
    THURM_MODE_BRACKETED_PASTE = 1 << 4,
    THURM_MODE_MOUSE_MOTION = 1 << 6,
    THURM_MODE_FOCUS_IN_OUT = 1 << 11,
    THURM_MODE_ALT_SCREEN = 1 << 12,
    THURM_MODE_MOUSE_DRAG = 1 << 13,
    THURM_MODE_MOUSE_ANY = (1 << 3) | (1 << 6) | (1 << 13),
};

typedef struct {
    uint32_t image;
    uint32_t placement;
    int32_t row; /* viewport row of the top-left cell, may be negative */
    int32_t col;
    uint32_t x_offset; /* pixels inside the top-left cell */
    uint32_t y_offset;
    uint32_t src_x;
    uint32_t src_y;
    uint32_t src_w;
    uint32_t src_h;
    uint32_t cols; /* destination size in cells */
    uint32_t rows;
    int32_t z; /* < 0 below text, >= 0 above */
    uint32_t dst_w; /* destination size in pixels when non-zero (overrides cols/rows) */
    uint32_t dst_h;
} thurm_image_placement;

/* Lock the pane's grid for reading. On success fills `info`, points `cells` at
 * rows*cols cells in row-major order and returns true. Must be paired with
 * thurm_grid_unlock(). Returns false when no frame has been received yet. */
bool thurm_grid_lock(thurm_client *client, thurm_pane_id pane, thurm_grid_info *info,
                     const thurm_cell **cells);

/* While locked: the full grapheme cluster at (row, col) when it has combining characters
 * (emoji ZWJ sequences, accents), else NULL. */
const char *thurm_grid_cluster(thurm_client *client, thurm_pane_id pane, uint16_t row,
                               uint16_t col);

/* While locked: the history line just above the viewport, for smooth scrolling. Points
 * `cells` at it and returns its length (cols), or 0 when there is none. */
size_t thurm_grid_peek(thurm_client *client, thurm_pane_id pane, const thurm_cell **cells);

/* While locked: grapheme cluster of the peek line at `col`, else NULL. */
const char *thurm_grid_peek_cluster(thurm_client *client, thurm_pane_id pane, uint16_t col);

typedef struct {
    uint16_t row;
    uint16_t col;
    const char *text; /* valid while locked */
} thurm_cluster;

/* While locked: every grapheme cluster in the grid. Copies up to `max` into `out` and returns
 * the total (call with max = 0 to size the buffer). Cheaper than thurm_grid_cluster per cell. */
size_t thurm_grid_clusters(thurm_client *client, thurm_pane_id pane, thurm_cluster *out, size_t max);

/* While locked: hyperlink URI for `link - 1`. */
const char *thurm_grid_link(thurm_client *client, thurm_pane_id pane, uint16_t index);

/* While locked: copy up to `max` image placements into `out`, returns the number copied. */
size_t thurm_grid_images(thurm_client *client, thurm_pane_id pane, thurm_image_placement *out,
                         size_t max);

void thurm_grid_unlock(thurm_client *client, thurm_pane_id pane);

/* RGBA8 (straight alpha) pixels of a kitty graphics image. Borrowed until
 * thurm_image_unlock(). Returns false if the image is not (yet) known. */
bool thurm_image_lock(thurm_client *client, thurm_pane_id pane, uint32_t image,
                      uint32_t *width, uint32_t *height, const uint8_t **rgba);
void thurm_image_unlock(thurm_client *client, thurm_pane_id pane, uint32_t image);
/* Identifies the pixels currently stored for `image`: it changes when the image is
 * replaced under the same id. 0 if the image is not (yet) known. */
uint64_t thurm_image_serial(thurm_client *client, thurm_pane_id pane, uint32_t image);

/* ---------------------------------------------------------------------------------------
 * Configuration.
 * ------------------------------------------------------------------------------------- */

/* Load ~/.config/thurm/config.toml (creating a commented default on first run) and return
 * the UI JSON: {"config":{...},"theme":{...},"theme_spec":{"light":"...","dark":"..."},
 * "themes":[{"name":"...","dark":true},...],"font_features":[...],"agent_presets":[...],
 * "config_path":"..."} or {"error":"...","config":{defaults...},...}. `theme` is resolved for
 * the `dark` system appearance. Free the result. */
char *thurm_config_json(bool dark);

/* Show every pane in theme `name` (a theme name or file) without saving it; NULL goes back to
 * the configured theme. Returns the theme shown as JSON (like "theme" in thurm_config_json), or
 * NULL for an unknown theme (nothing changes). Free the result. */
char *thurm_preview_theme(thurm_client *client, const char *name);

/* Stop the running daemon even if it speaks another protocol version (SIGTERM: it saves the
 * session first). Blocks until it exited. Use when Hello is refused after an update. */
bool thurm_terminate_daemon(void);

/* Replace the running daemon with `daemon_path` in place: it execs the new binary and keeps
 * every pane's process. 0: the daemon now runs this build; 1: it predates in-place upgrades
 * (use thurm_terminate_daemon); 2: failed. Blocks up to ~15 s. */
int32_t thurm_upgrade_daemon(const char *daemon_path);

/* This build's identifier, compared with the "build" of the Hello response. Static string. */
const char *thurm_build_id(void);

/* Protocol version to send in the Hello request. */
uint32_t thurm_protocol_version(void);

/* Path of the config file. Free the result. */
char *thurm_config_path(void);

/* ---------------------------------------------------------------------------------------
 * Remote workspaces (`[[remote]]` in the config).
 * ------------------------------------------------------------------------------------- */

/* A host's tunnel state changed: {"name":"devbox","host":"devbox","phase":"connected"|
 * "connecting"|"reconnecting"|"needs_attention"|"not_installed"|"upgrade_needed"|"disabled"|
 * "stopped","message":...,"socket":"<local socket>","retry_at":<unix s>|null,
 * "remote_build":...,"remote_protocol":...,"upgrade_available":bool,"os":...,"arch":...,
 * "linger":...}. Runs on a background thread. Connect with thurm_connect_socket(socket)
 * once the phase is "connected"; the tunnel reconnects by itself otherwise. */
typedef void (*thurm_remote_status_cb)(void *ctx, const char *json);

/* Start a supervised ssh tunnel per enabled [[remote]]. Background retries never install
 * anything on a host. */
void thurm_remotes_start(thurm_remote_status_cb on_status, void *ctx);
/* Pick up added, removed or changed [[remote]] entries. */
void thurm_remotes_sync(void);
/* Close every tunnel (the remote daemons keep their panes). */
void thurm_remotes_stop(void);
/* Retry `name` (NULL: all) now; with `restart`, also replace a tunnel that stopped answering
 * (wake from sleep, network change). */
void thurm_remote_kick(const char *name, bool restart);

/* One remote operation, JSON in and out ({"error":"..."} on failure). Free the result.
 * Operations that talk to a host run ssh and block: call them off the main thread.
 *   {"op":"status"}                                       [status, ...]
 *   {"op":"plan","name":"devbox","bins":"<Helpers dir>"}  {"methods":["copy"|"download"|"nix"],
 *        "labels":[...],"problem":...,"installed":bool,"host":{...},"daemon":{"running":bool,
 *        "protocol":n,"build":...,"hot_upgrade":bool}|null}
 *   {"op":"install","name":"devbox","method":"download","bins":"..."}   {"build":...,...}
 *   {"op":"upgrade_daemon","name":"devbox","allow_restart":false}
 *        {"ok":true,"output":...} or {"ok":false,"would_stop_panes":true,"protocol":n}
 *   {"op":"add","name":"devbox","target":"me@devbox"}     {"name":...,"host":{...}} (checks ssh,
 *        writes [[remote]]; installing is doctor_fix "thurm")
 *   {"op":"doctor","name":"devbox","bins":"..."}          {"plan":{...},"checks":[{"id":...,
 *        "title":...,"state":"ok"|"warn"|"fail"|"skip","detail":...,"fix":{"label":...,
 *        "confirm":...|null}|null,"terminal":"<script>"|null}, ...]}
 *   {"op":"doctor_fix","name":"devbox","id":"linger","bins":"...","allow_restart":false}
 *        {"output":...}
 *   {"op":"terminal_argv","name":"devbox","script":"..."}  ["ssh", ..., "-t", ...] (run it in a pane)
 *   {"op":"allow_clipboard","name":"devbox"}              "Ok" (clipboard_read = "always")
 *   {"op":"clipboard_read","host":"devbox"|null}          "allow" | "deny" | "ask"
 *   {"op":"clipboard_write","host":"devbox"|null}         "allow" | "deny"
 *   {"op":"link","host":"devbox"|null,"url":"..."}        {"action":"open"|"ask"|"ignore"} or
 *                                                         {"action":"copy_path","path":"..."}
 *   {"op":"handoff_prepare","host":"devbox","path":"<repo>","branch":null}   handoff
 *   {"op":"handoff_set_pane","id":"...","pane":7}          handoff
 *   {"op":"handoff_fetch","id":"..."}                     handoff (fetched, fetch_error)
 *   {"op":"handoff_check","id":"..."}                     {"uncommitted":bool,"unfetched":bool,...}
 *   {"op":"handoff_cleanup","id":"...","force":false}     {"branch_deleted":bool,"message":...}
 *   {"op":"handoff_defer","id":"..."}                     handoff (pending_cleanup)
 *   {"op":"handoff_list"}                                 [handoff, ...]
 * A handoff: {"id":"devbox/<repo id>/<slug>","host":...,"repo":"<local path>","branch":
 * "agent/<slug>","worktree":"<remote path>","git_remote":"thurm-devbox","pane":n|null,...} */
char *thurm_remote_call(const char *json);

#ifdef __cplusplus
}
#endif

#endif /* THURM_H */
