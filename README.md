# horizon-plugin-host

> Unofficial plugin host for the Horizon server (https://github.com/horizon-rust/horizon). The plugin names it lists belong to independent ports; it is not affiliated with, or endorsed by, the authors of the originals.

A plugin host for [Horizon](https://github.com/horizon-rust/horizon) dedicated servers: a native trait mod,
built into the server once, that loads server plugins as shared libraries (`cdylib`) from `plugins/` and
reloads them while players stay on, the way Paper's plugin managers do:

```
/plugin list
/plugin load <name>
/plugin unload <name>
/plugin reload <name>
```

Operator level 4, in game and on the console. Every sub-command is a literal in the usage tree, so the chat
completes it. `/plugins` (alias `/pl`) lists every installed port for everyone, in Paper's format
(`Server Plugins (N):`, then the names in green, sorted, deduplicated). It shows the trait mods' ids (`mod_ids()`,
mapped to display names) plus the extra ports from `plugins/pluginhost/extra-plugins.txt` (one per line; the
defaults in `host/api/src/lib.rs` when the file is absent). A mixin-only mod does not show.

## Pieces

| Crate | What |
|---|---|
| `abi/` (`horizon-plugin-abi`) | the `#[repr(C)]` tables both sides share, and the plugin SDK (`ServerPlugin`, `export_plugin!`); `std` only |
| `loader/` (`horizon-plugin-loader`) | finds, maps, starts, reloads and unloads plugins and dispatches events; engine-free, tested without a server |
| `host/` (`pluginhost`) | the Horizon trait mod: game hooks to loader calls, loader calls to the game |
| `examples/greeter/` | an example plugin |

## Writing a plugin

```rust
use horizon_plugin_abi::*;

#[derive(Default)]
struct Hello;

impl ServerPlugin for Hello {
    const NAME: &'static str = "hello"; // must match the file name, plugins/hello.so
    const VERSION: &'static str = "1.0.0";
    fn commands(&self) -> Vec<CommandSpec> {
        vec![CommandSpec::new("hello", 0), CommandSpec::new("hello-admin", 3).node("hello.admin")]
    }
    fn command(&self, cx: &mut Cx<'_>, _sender: Option<&PlayerInfo<'_>>, line: &str) -> bool {
        cx.reply(&format!("you ran /{line}"))
    }
    fn chat(&self, cx: &mut Cx<'_>, _p: &PlayerInfo<'_>, message: &str) -> Flow {
        if message.contains("spam") { Flow::Cancel } else { Flow::Continue }
    }
}

export_plugin!(Hello, Hello::default());
```

`crate-type = ["cdylib"]`, build with `cargo build --release`, copy `target/release/libhello.so` to the server's
`plugins/hello.so` (`libhello.dylib` and `plugins/hello.dylib` on macOS), then `/plugin reload hello`. See
`examples/greeter` for every event.

## Events and host functions (ABI 1)

| Plugin event | Cancel | Horizon hook |
|---|---|---|
| `command` (usage, operator level, optional permission node) | taken / not taken | `perform_command`, `perform_console_command`, `player_command` |
| `player_joined` | | `player_logged_in` |
| `player_quit` | | `player_logging_out` (feature `hooks-all`), else `player_logged_out` |
| `chat` | yes, and `rewrite_chat` | `player_chat` |
| `tick`, scheduled `task`s | | `server_tick` (start phase) |
| `block_break` | yes | `destroy_block` (player = the acting player) |
| `block_place` | yes | `block_place` (feature `hooks-all`) |
| `player_damage` (between players; amount can change) | yes | `player_hurt_by_player` (feature `hooks-all`) |
| `console_line` (console or RCON, before any command) | yes | `console_line` (feature `hooks-all`) |

Host functions: `log`, `reply`, `tell`, `broadcast`, `run_command`, `has_permission(player, node, default_level)`,
`schedule(id, delay, period)` / `cancel_task`, `config_dir` (`plugins/<name>/`, made before the plugin starts),
`rewrite_chat`.

Feature `hooks-all` (on by default) needs hooks that are not merged upstream yet; without it the host builds against
upstream Horizon and the rows marked above are missing (permission nodes then never grant; levels still do, but a
joining or leaving player's level reads as 0 to plugins, and new commands wait for a restart). Hooks Horizon lacks
are listed in [GAPS.md](GAPS.md).

## The ABI

- A plugin exports `horizon_server_plugin_v1: extern "C" fn() -> *mut PluginV1`. The major version is in the
  symbol's name: a plugin of another major is not found, and nothing of it is called.
- Only `#[repr(C)]` values cross: borrowed UTF-8 strings valid for one call, UUIDs as two `u64`s, integers. No
  `String`, `Box`, `Vec` or trait object crosses and neither side frees the other's memory, so the plugin's own
  `std` and allocator never meet the game's.
- Both tables start with `size`. Fields are only appended within a major, and each side calls a field only when the
  other side's `size` covers it (`field_end!`, `HostV1::has`, `PluginV1::has`). A plugin built against an older
  minor runs on a newer host and the other way round; `Cx` returns `None`/`false` for host functions the host
  does not have.
- Panics stop at the boundary: the SDK wraps each call in `catch_unwind`; a panic is "not handled"/"continue".
- Cancellable events go to plugins in load order (alphabetical at start); the first that cancels ends the event.

## Load, reload, unload

- **Load**: `plugins/<name>.so` (`.dylib` on macOS) is copied to `plugins/.loaded/<name>-<n>.so`, mapped, and the copy's name removed
  (the mapping keeps it), so a rebuilt file can be dropped in place while the old copy runs and a second load is a
  fresh library. `libloading` maps with `RTLD_LOCAL`: two copies' symbols never clash.
- **Reload**: the new file is mapped first; one that is not a plugin of this ABI leaves the running one in place.
  Then the old plugin's `unload` (its `Drop` runs, its tasks go), then the new entry point. A plugin never runs
  twice at once.
- **Never unmapped**: an unloaded plugin's library stays mapped until the process exits. Unmapping Rust code is not
  sound: a thread it started, a thread-local's destructor or a `'static` it handed out would point into freed
  memory. Each reload costs one mapping.
- State is dropped on reload: what must survive goes to `plugins/<name>/`. Threads a plugin starts must be stopped
  and joined in `Drop`.

## Limits

- A command a plugin adds after the start runs for level-0 players at once (through `player_command`). With
  `hooks-all` the host then asks the game to rebuild the command tree (`refresh_commands`), so every player gets the
  command at the next tick; without it, clients complete the command and operator-only commands work only after a
  restart, since the game builds the command tree once.
- Hooks with no `Host` in the game (chat, hits) queue `tell`/`broadcast`/`run_command` for the next tick; a chat
  rewrite cancels the signed line and broadcasts the new text as a system line (no chat-decorate hook yet, see
  [GAPS.md](GAPS.md)). Their players read as level 0 to plugins.
- Native plugins are full-trust code with the server's rights. Nothing is sandboxed.
- Linux and macOS. On Windows a mapped copy cannot be removed, so `.loaded/` keeps the copies.

## Tests

```
cargo test -p horizon-plugin-abi
cargo test -p horizon-plugin-loader     # builds a fixture plugin with cargo --offline
```

## License

MIT, see [LICENSE](LICENSE).

## Contributing

Pull requests and improvements are always welcome! Open an issue or a PR -- direct pushes to main are disabled, every change goes through review.
