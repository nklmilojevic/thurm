# Changelog

## v0.26.0 - 2026-11-22

This release adds Pi and Antigravity CLI (`agy`) support to Thurm. Thurm now tracks the session state of both agents.

### New features

- Thurm adds managed hooks, skills, and pane icons for Pi and Antigravity. You can fork and restore Pi sessions, and restore `agy` conversations.

## v0.25.0 - 2026-11-18

Thurm 0.24.0 opens remote dev servers on this Mac and copies files to and from remote hosts. It also adds a sidebar list of tabs for every host.

### New features

- Remote hosts now carry byte streams through the daemon connection. Port forwards and file copies use these streams.
- Command-click on a link to a port on a remote host now opens a forward on this Mac. Set `forward_links = false` in a `[[remote]]` entry to open such links on this Mac.
- Drop or paste files on a remote pane to upload them to the host. Quick Look previews remote files, and their file links offer Download.
- Plugins receive port, upload and forward events. Plugins can also open and close forwards.
- Clipboard tools such as `pbcopy`, `xclip` and `wl-copy` on a remote host now reach the Mac clipboard. Run `thurm remote doctor` on a Linux host to install them.
- Run `thurm cp` to copy files between this Mac and a remote host. Run `thurm remote forwards` to list open forwards and their connections.
- The selected sidebar row and tab have a raised pill tinted from the theme background. It replaces the faint grey fill.
- `thurm remote add NAME ADDRESS` takes one address for an ssh or tailnet host. Thurm refuses an address that is already saved under another name.
- A hosts button in the sidebar lists the tabs of every host, each under its own header. Turn it on with View > Tabs by Host or `window.sidebar_by_host`. It is off by default.

## v0.24.0 - 2026-11-16

This release lets you run a command out of sight in a minimized card. You can also move the chips of minimized cards where you need them.

### New features

- Use `thurm new-float --minimized` to start a command in a minimized card, and `thurm float --minimize` or `--restore` to minimize or restore a pane. Drag a chip within its tab, or onto the sidebar to make it a tab, and it stays there.

## v0.23.1 - 2026-11-15

This release fixes two problems with held panes. Held panes now stay open after their command exits, and they no longer show a stale agent state.

### Fixes

- The macOS app keeps a held pane open after its command exits. Before, a pane started with `--hold` closed at once, so you could not read its output.
- A held pane no longer keeps its agent after the process exits. Finished or waiting agents no longer show on the Dock badge, hover card, or agent lists.
- The tailnet component now includes security fixes in its Go modules. The fixes cover compression, crypto, and networking packages.

## v0.23.0 - 2026-11-14

This release lets you preview a file path in Quick Look with one click. It is available in the macOS app.

### New features

- Press `Cmd+Option` and click a file path to open it in Quick Look. This is the same panel as the context menu. It also works on `file:` links, and dragging panes or floating cards still moves them.

## v0.22.0 - 2026-11-14

Thurm 0.21.0 lets plugins place the floating pane they open. Plugin actions also get more context, and a new example plugin previews files with Quick Look.

### New features

- A plugin sets `frame` or `cells` in `panes.open` to place the floating pane it opens. Older Thurm versions also need `float = true`, or they open a tab.
- Plugin actions get `ctx.selection`, `ctx.file`, and `ctx.remote`. A plugin can act on the selected text, the file under the pointer, or the remote host.
- `thurm.fail(message)` stops an action with a message. The user sees the message alone, without a file name and line number.
- The new `quicklook` example plugin previews a file in a floating pane. Press `cmd+y` to preview the selected file or the file under the pointer.

### Fixes

- `panes.open` no longer makes a pane when its target pane does not exist.
- Actions read the pointer position when they run. The `ctx.file` value is the file under the pointer at that time.
- `panes.open` falls back to a default float only for an app too old to place panes.

### Other changes

- The plugin guide documents pane placement and the new action context fields. The Lua types list them too.

## v0.21.0 - 2026-11-14

Thurm adds an extension API with actions, key bindings and events. Lua plugins and program plugins use it, and `thurm plugin` manages them.

### New features

- The daemon adds an extension API with actions, key bindings and events. Use `thurm action list` and `thurm action run` to call actions from the command line.
- The iOS app shows the time of each agent's last report. Agents appear in lists the same way as on the Mac.

### Fixes

- An idle agent that re-sends its terminal title no longer shows the status "Working". Writes that only set the title do not count as pane activity.
- The agent list shows a permission prompt first, even when the agent is Working or Idle. A prompt with no message shows "Needs permission".
- A Live Activity timer now counts from when the agent's status began. Before, it counted from when the update arrived, so it showed too little time.

## v0.20.1 - 2026-11-13

This release fixes a macOS crash. The app now stays open when you switch from the sidebar to native tabs.

### Fixes

- The app no longer crashes when you use View > Tabs in Sidebar (⌃⌘S) to switch to native tabs. The titlebar now updates in the correct order.

## v0.20.0 - 2026-11-13

This release gives you new ways to move, minimize and find panes. It also groups agents by status in the sidebar.

### New features

- You can minimize a floating pane to a chip from its header. Click the chip to restore the pane where it was.
- Hold Command and Option, then drag a pane. Drop it at a tab edge to give it half the tab. Press Esc to cancel. View > Move Pane to Edge gives the same moves in the menu.
- Set `font.style` to a face name, such as `Semi-Condensed`, for regular text. Bold and italic text then keep the same width. An unknown style leaves the font faces unchanged.
- Only Active Tabs groups agents by status: Needs input, Failed, Done and Working. Each agent that is not idle gets its own row.
- The Agents panel is removed. Old `window.sidebar_agents` settings still load. The `toggle_sidebar_agents` actions now toggle Only Active Tabs.
- Each agent row shows the activity text that the agent reports. The row also shows how long ago the agent last reported.
- Use the search picker to find workspaces, tabs, panes and agents.
- Use the Insert Path picker to add a file path to the terminal.
- Preview a file path in the terminal with Quick Look.

### Fixes

- Tabs from the CLI open in the workspace of their source pane.
- Background tabs and restored tabs keep their creation order.
- For agents whose hooks miss turns, the screen now sets the agent status.
- A finished turn's prompt no longer returns after Stop.
- Skill setup now copies skill files into place. Broken links are replaced by local copies.
- Split titles no longer start pane drags on hover. The titlebar still moves the window.

### Other changes

- The documentation covers minimized panes, edge moves and the activity list.

## v0.19.4 - 2026-11-10

Thurm now clears the Dock badge for a pane that you close. This release fixes a bug that kept a finished agent in the badge count.

### Fixes

- The Dock badge clears when you close a pane that ran an agent. Before this fix, a closed pane could come back and keep its finished agent in the badge count until you restarted the app.

## v0.19.3 - 2026-11-10

This release fixes notifications that stayed in Notification Center after you read their pane.

### Fixes

- Notifications now leave Notification Center after you read their pane. Before, program notifications, finished commands and failed commands could stay. The daemon now marks these as unread. The macOS app also removes notifications of panes on screen when focus changes.

## v0.19.2 - 2026-11-08

This release fixes the motion of the sidebar toggle. Panes keep their size during the slide and resize once it ends.

### Fixes

- The sidebar toggle uses the standard motion again. The Agents panel fold uses the same motion.
- Panes no longer resize before the sidebar slide. Text no longer wraps and fades in the middle of the slide.
- Full-screen programs no longer flash their old layout after a resize. The terminal holds the frame until the program redraws.

## v0.19.1 - 2026-11-07

This release updates the terminal engine to libghostty-vt 1.0.0. Your terminal sessions behave as before.

### Other changes

- Thurm now builds against libghostty-vt 1.0.0, the first release with a stable C interface. Styles, underline colors, and scrollback history keep working as before.

## v0.19.0 - 2026-11-07

This release adds tab history and a fold for the Agents panel. It also fixes how a pane clears its selection.

### New features

- Back (`Cmd+[`) and Forward (`Cmd+]`) move through the tabs you visited. The mouse back and forward buttons do the same, unless `window.mouse_navigation_buttons` is `false`.
- Click the Agents header to fold the panel and give the tab list more space. Use View > Collapse Agents to do the same from the menu.
- A client can claim panes, so that they keep its size. This helps the iOS app keep every pane on screen at its own size.

### Fixes

- A selection clears when you type, paste, or press Esc in a pane. The selection also clears in a pane where the program uses the mouse.

## v0.18.0 - 2026-11-06

Thurm now lets you pin tabs, move tabs between workspaces, and drag tabs into cards. It also fixes Neovim pickers, titlebar zoom, and pane borders in light themes.

### New features

- The sidebar menu has `Pin Tab` and `Unpin Tab`. Pinned tabs stay at the top of the sidebar in every workspace, even after a restart.
- `Move Tab to Workspace` sends a tab to another workspace on the same host. The tab keeps its splits, its cards, and its programs.
- Drag a tab from the sidebar onto the terminal area to make it a card. Drag a card header onto the sidebar to make it a tab again.
- The View menu and the command palette also offer these actions.
- The workspace button now opens a compact picker. Type a name or remote host to filter the list, or use the keyboard to choose.

### Fixes

- Neovim pickers no longer flash in the editor pane. A picker now appears in its card only after it is drawn.
- Double-click on the titlebar now zooms the window. The action follows the setting in System Settings > Desktop & Dock.
- Inactive pane borders stay visible in light themes. The border now shows on all four sides of the pane.

## v0.17.0 - 2026-11-02

This release gives you floating panes that open at an exact place and size. It also adds `thurm-nvim`, which runs Neovim in Thurm panes, and fixes macOS sidebar and resize problems.

### New features

- `thurm new-float` and `thurm float` now accept `--frame` and `--cells`. You can open or move a floating pane at an exact place and size.
- `thurm-nvim` runs Neovim with one Thurm pane for each of its windows. Buffers, registers, undo history and language servers are shared by all panes.
- Pickers in `thurm-nvim`, such as Snacks, open as floating cards. Focus moves into the card, so you can filter the list with the keyboard.

### Fixes

- The macOS sidebar no longer shows a strip of a different color at the edge of a pane. Terminal text resizes smoothly when the sidebar opens or closes.
- The New Tab button stays in the macOS toolbar when the sidebar is hidden. Before, it could move into the overflow menu.
- Command palette shortcuts on macOS now use the system font. Each modifier symbol has more space after it, so the shortcuts are easier to read.

## v0.16.0 - 2026-10-29

This release adds three sidebar settings to the macOS app. It also fixes agent icons behind PTY proxies, sidebar drawing for new tabs, and keyboard focus after a tab closes.

### New features

- Adds the `window.sidebar_group_by_repo`, `window.sidebar_all_workspaces` and `window.sidebar_active_only` settings. They change which tabs the sidebar lists. The selected row now uses a capsule in the theme's selection color.

### Fixes

- The sidebar shows the agent icon when `atuin pty-proxy` runs the shell. Before, it showed the shell icon.
- New tabs draw their sidebar at once. Before, the first frames listed only the new tab.
- Typing goes to the next tab after you close a tab. Before, keys went nowhere until you clicked the terminal.

## v0.15.0 - 2026-10-28

This release adds floating panes, which show a pane above the splits of a tab. It also compresses remote output and fixes input and connection problems.

### New features

- Floating panes: a pane can float above the splits of a tab as a card. Use View › Float Pane, the pane header button, or `thurm float`. Dock Pane puts the card back into the splits.
- Cmd+Option+Return opens a shell as a card on any tab, or shows and hides the cards.
- The `thurm new-float` command opens a pane as a card.
- The focus shortcuts now move to floating cards too.
- Right-click a link to choose Open Link or Copy Link. Open Link uses the same policy as Cmd-click.
- Copy Pane ID in the Shell menu copies a pane ID for use with `thurm --pane`.
- Remote clients, such as the iOS app and the web client, now receive compressed terminal output. This sends less data over the network.

### Fixes

- A click that closes the command palette no longer reaches the window below it.
- A click in a pane closes the tab-completion popup.
- ⌘1–⌘9 now work on keyboard layouts that need Shift for digits, such as AZERTY. Custom `[keybindings]` still apply.
- A program stopped with Ctrl+C no longer shows as working in the sidebar, even without shell integration.
- Busy remote connections, such as phones, no longer close while output flows. Only connections that send and receive nothing are closed.

### Other changes

- Building from source now needs Zig 0.17.
- The terminal engine, libghostty-vt, now uses a newer Ghostty version.

## v0.14.0 - 2026-10-27

Thurm now moves panes with a smoother, faster motion and a new `window.animation_speed` setting. A phone keeps the size of the pane it shows.

### New features

- Pane moves now use a critically damped spring. The default motion is faster than before, and zoom uses the same curve.
- The `window.animation_speed` setting adds `normal`, `slow` and `slower`. Use `slow` or `slower` for a longer motion.
- A pane shown on the phone keeps the phone's size. A layout change on the Mac no longer resizes it. A key press on the Mac takes the size back.
- The focus ring uses the theme's cursor color when the theme sets a distinct one. Themes without one keep blue.

### Fixes

- Pane moves no longer drop frames when the Mac is busy. The render server now plays each move, so the app does less work.

## v0.13.0 - 2026-10-24

This release adds an Agent Wall and an active pane border. It also fixes a bug that quit the app when a workspace lost its last tab.

### New features

- Thurm now shows a notice when the session daemon is an older build, locally or on a remote host. The notice offers a restart or an in-place upgrade, so new features work.
- Agent Wall moves every agent in a window into one grid tab, without a restart. Agents keep running and return to their tabs when you turn the wall off.
- A new option, `window.active_pane_border`, outlines the focused pane in the accent color. The option is off by default. It shows which pane has focus in a split tab.

### Fixes

- When a workspace loses its last tab, Thurm shows the most recently used workspace. Before, the app quit while other workspaces still had tabs.
- The sidebar tab shortcut, `⌘N`, now appears on the branch row only while Command is held. Long branch names use the full row width when the shortcut is hidden.
- `thurm send` now sends Enter as a separate write after the text. Claude Code no longer reads the Enter as a new line, so messages submit.

## v0.12.1 - 2026-10-24

This release fixes a macOS problem where clicks in the tab hover card did nothing.

### Fixes
- The tab hover card now takes clicks on its map and labels. Before this release, a click on a pane did nothing. Now, a click on a pane picks that pane.

## v0.12.0 - 2026-10-24

This release lets you go to a pane from the tab hover card on macOS. This release also fixes the pane header buttons and the mouse reports.

### New features

- The tab hover card on macOS now takes mouse clicks. A click on a pane in the map or the list selects its tab and focuses that pane.

### Fixes

- The split, zoom and close buttons in a pane header have a larger click area. A click near a button no longer focuses the pane by mistake.
- Mouse reports keep the correct format after a re-attach. Programs such as Textual no longer get every click at the top-left cell.

## v0.11.0 - 2026-10-22

Thurm now gives agents and scripts more control over panes. It also adds shell completions and fixes stray text in attached copies.

### New features

- Agents get the guide from the `thurm` they run, and `thurm skill setup` installs it for Claude Code, Codex, OpenCode and Pi. The CLI also gains layout commands for background panes, split sizes and cross-tab moves.
- Thurm now prints shell completion scripts for bash, elvish, fish, powershell and zsh with `thurm completion <shell>`. Your shell then completes `thurm` commands and options.
- Tab asks supported programs for their own completions, so they stay current. Bundled specs now cover sofka, vesnik, enklava, sefko, thurm, codex, mailerlite and mailersend.

### Fixes

- Attached copies no longer show stray text, such as `1m`, after you switch back to a window.
- Prompt marks in replayed zsh output now appear at the correct column.

### Other changes

- Thurm now uses a newer lock library. Locks that many threads share perform better under contention.
- To build Thurm from source, you now need Rust 1.95 or later.

## v0.10.1 - 2026-10-21

This release fixes a security advisory in a certificate library and restores the CI build.

### Fixes

- The v0.10.0 release built no artifacts because CI failed. This fix updates `rustls-webpki` to 0.103.16 (RUSTSEC-2026-0382) and makes CI pass.

## v0.10.0 - 2026-10-20

This release lets you run Thurm with a headscale coordination server. It also lets you pair a phone when HTTPS is off on your tailnet.

### New features

- Set `tailscale.control_url` to sign in to headscale or another coordination server. Set `tailscale.plain_listen` to pair a phone when HTTPS is off. The Pair iPhone sheet now checks the command-line tool and shows how to install it. A new Linux server guide and `install.sh` script install Thurm on Linux.
