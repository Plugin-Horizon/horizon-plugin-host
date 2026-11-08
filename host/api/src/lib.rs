//! Plugin host: a trait mod that loads server plugins from the server's `plugins/` folder
//! (`horizon-plugin-loader`) and reloads them while the server runs. `/plugin list|load|unload|
//! reload <name>` for operators of level 4 and the console. `/plugins` (alias `/pl`) lists the
//! Horizon mods for everyone, as Paper's `/plugins` lists its plugins.
//!
//! Hooks without a `Host` (chat, hits between players) cannot reach the game during the call, so
//! what a plugin does there through the host (`tell`, `broadcast`, `run_command`) is queued and
//! done at the start of the next server tick.
#[cfg(not(feature = "hooks-all"))]
use horizon_client::mod_api::PlayerLoggedOutEvent;
#[cfg(feature = "hooks-all")]
use horizon_client::mod_api::{
    BlockPlaceEvent, ConsoleLineEvent, PlayerHurtByPlayerEvent, PlayerLoggingOutEvent,
};
use horizon_client::mod_api::{
    DestroyBlockEvent, Flow, Hooks, Host, NativeCommand, PerformCommandEvent,
    PerformConsoleCommandEvent, PlayerChatEvent, PlayerCommandEvent, PlayerLoggedInEvent,
    RegisterCommandsEvent, ServerStoppingEvent, ServerTickEvent, ServerTickPhase, Text,
};
use horizon_client::text;
use horizon_plugin_loader::{BlockAt, Calls, Chat, Plugins, Sender, root};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub fn hooks() -> Box<dyn Hooks> {
    Box::new(PluginHost::default())
}

/// The `/plugin` tree: every sub-command a literal, so the client completes it. Operators of
/// level 4 in game; the console runs all of them.
const PLUGIN_USAGES: [&str; 5] = [
    "plugin",
    "plugin list",
    "plugin load <name:word>",
    "plugin unload <name:word>",
    "plugin reload <name:word>",
];

/// Ports that are not trait mods (their ids never reach `mod_ids()`), shown by `/plugins` when
/// `plugins/pluginhost/extra-plugins.txt` is absent. One name per line in that file; `#` starts a
/// comment.
const DEFAULT_EXTRA_PLUGINS: &[&str] = &[
    "Chunky",
    "DiscordSRV",
    "Floodgate",
    "HuskSync",
    "InvSee++",
    "LuckPerms",
    "SkinsRestorer",
    "squaremap",
    "TAB",
    "ViewDistanceTweaks",
    "voicechat",
    "WorldEdit",
];

/// The version of each port, as its `horizon-mixin.toml` declares it (`/version <plugin>`; a mod
/// cannot read its own bundle's version at run time). Keep in step with the bundles.
const PORT_VERSIONS: &[(&str, &str)] = &[
    ("Chunky", "0.1.0"),
    ("Citizens", "0.1.0"),
    ("CombatLog", "0.2.0"),
    ("DiscordSRV", "0.3.0"),
    ("Floodgate", "0.1.0"),
    ("HuskSync", "0.1.0"),
    ("InvSee++", "0.1.0"),
    ("LuckPerms", "0.1.0"),
    ("PluginHost", "0.2.0"),
    ("Sentinel", "0.1.0"),
    ("SkinsRestorer", "0.2.0"),
    ("squaremap", "0.3.0"),
    ("TAB", "0.1.0"),
    ("ViewDistanceTweaks", "0.2.0"),
    ("voicechat", "0.1.0"),
    ("WorldEdit", "1.2.1"),
    ("WorldGuard", "0.1.0"),
];

/// The roots that answer `<root> <plugin>` like Paper's `/version <plugin>` (`/ver`, `/about`).
fn is_version_root(r: &str) -> bool {
    matches!(r, "version" | "ver" | "about")
}

/// Paper's `/version <plugin>` answer for `line`, None when it has no plugin argument (vanilla's
/// `/version` then answers). The name matches case-insensitively, `InvSee` for `InvSee++` too.
fn version_reply(line: &str) -> Option<Vec<String>> {
    let wanted = line.trim_start_matches('/').split_whitespace().nth(1)?;
    let key = wanted.to_lowercase();
    let key = key.trim_end_matches('+');
    let found = PORT_VERSIONS
        .iter()
        .find(|(n, _)| n.to_lowercase().trim_end_matches('+') == key);
    Some(match found {
        Some((name, version)) => vec![
            format!("{name} version {version}"),
            format!("{name} is a Horizon port; it runs inside the server as a native mod."),
        ],
        None => vec![
            "This server is not running any plugin by that name.".to_owned(),
            "Use /plugins to get a list of plugins.".to_owned(),
        ],
    })
}

/// A trait mod's `[mod] id` as its display name; unknown ids stay as they are.
fn display_name(id: &str) -> &str {
    match id {
        "citizens" => "Citizens",
        "combatlog" => "CombatLog",
        "worldguard" => "WorldGuard",
        "pluginhost" => "PluginHost",
        "sentinel" => "Sentinel",
        "voicechat" => "voicechat",
        other => other,
    }
}

/// The extra ports: the file's names, else the defaults.
fn extra_plugins(host: &mut dyn Host) -> Vec<String> {
    let file = host.config_dir().ok().and_then(|c| {
        c.parent()
            .map(|base| base.join("plugins/pluginhost/extra-plugins.txt"))
    });
    match file.and_then(|f| std::fs::read_to_string(f).ok()) {
        Some(text) => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_owned)
            .collect(),
        None => DEFAULT_EXTRA_PLUGINS
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    }
}

/// Paper's `/plugins` (alias `/pl`): every installed port, as Paper lists its plugins
/// (`Server Plugins (N):`, then the names in green, comma-separated). For everyone. The trait mods'
/// ids and the extra list, deduplicated case-insensitively and sorted the same way.
fn plugins_list(host: &mut dyn Host) {
    let mut names: Vec<String> = horizon_client::mod_api::mod_ids()
        .iter()
        .map(|id| display_name(id).to_owned())
        .chain(extra_plugins(host))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    host.feedback(Text::literal(format!("Server Plugins ({}):", names.len())));
    if names.is_empty() {
        return;
    }
    let mut line = Text::empty();
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            line.push(Text::colored(", ", text::WHITE));
        }
        line.push(Text::colored(name.as_str(), text::GREEN));
    }
    host.feedback(line);
}

#[derive(Default)]
struct PluginHost {
    plugins: OnceLock<Plugins>,
    /// Work from hooks without a `Host`, done on the next tick.
    later: Mutex<Vec<Later>>,
}

enum Later {
    Tell(u128, String),
    Broadcast(String),
    Command(String),
}

/// Where a plugin's answer to a command goes.
enum Sink<'a> {
    /// A player's command before the game reads it (`player_command`), or console replies.
    Replies(&'a mut Vec<Text>),
    /// A registered command's answer (`perform_command`, the console).
    Feedback,
    /// No command: the log.
    Log,
}

struct HostCalls<'a, 'h> {
    host: &'a mut (dyn Host + 'h),
    sink: Sink<'a>,
}

fn has_permission(host: &mut dyn Host, player: u128, node: &str, default_level: u8) -> bool {
    #[cfg(feature = "hooks-all")]
    {
        host.has_permission(player, node, default_level)
    }
    #[cfg(not(feature = "hooks-all"))]
    {
        // No permissions provider upstream: the caller already compared the level.
        let _ = (host, player, node, default_level);
        false
    }
}

/// The operator level of a player who is on or joining, as the game knows it; 0 with an upstream
/// build, which cannot tell.
fn player_level(host: &dyn Host, player: u128) -> u8 {
    #[cfg(feature = "hooks-all")]
    {
        host.player_permission(player).unwrap_or(0)
    }
    #[cfg(not(feature = "hooks-all"))]
    {
        let _ = (host, player);
        0
    }
}

/// After a plugin was loaded, reloaded or unloaded, the game rebuilds the command tree at the next
/// tick and sends it to everyone, so the new commands work for all players at once.
fn commands_changed(host: &mut dyn Host, line: &str) {
    #[cfg(feature = "hooks-all")]
    {
        if matches!(
            line.split_whitespace().nth(1),
            Some("load" | "reload" | "unload")
        ) {
            host.refresh_commands();
        }
    }
    #[cfg(not(feature = "hooks-all"))]
    {
        let _ = (host, line);
    }
}

impl Calls for HostCalls<'_, '_> {
    fn log(&mut self, plugin: &str, line: &str) {
        log::info!("[{plugin}] {line}");
    }
    fn reply(&mut self, plugin: &str, line: &str) -> bool {
        let text = Text::literal(line.to_owned());
        match &mut self.sink {
            Sink::Replies(lines) => lines.push(text),
            Sink::Feedback => self.host.feedback(text),
            Sink::Log => log::info!("[{plugin}] {line}"),
        }
        true
    }
    fn tell(&mut self, player: u128, line: &str) -> bool {
        self.host.tell(player, Text::literal(line.to_owned()))
    }
    fn broadcast(&mut self, line: &str) {
        self.host.broadcast(Text::literal(line.to_owned()))
    }
    fn run_command(&mut self, line: &str) -> bool {
        self.host.run_command(line)
    }
    fn has_permission(&mut self, player: u128, node: &str, default_level: u8) -> bool {
        has_permission(self.host, player, node, default_level)
    }
}

/// Calls from a hook without a `Host`: queued for the next tick.
struct Deferred<'a> {
    later: &'a Mutex<Vec<Later>>,
    /// Replies to the chatting player, when there are any.
    replies: Option<&'a mut Vec<Text>>,
}

impl Deferred<'_> {
    fn push(&self, item: Later) {
        if let Ok(mut later) = self.later.lock() {
            later.push(item);
        }
    }
}

impl Calls for Deferred<'_> {
    fn log(&mut self, plugin: &str, line: &str) {
        log::info!("[{plugin}] {line}");
    }
    fn reply(&mut self, _plugin: &str, line: &str) -> bool {
        match self.replies.as_deref_mut() {
            Some(replies) => {
                replies.push(Text::literal(line.to_owned()));
                true
            }
            None => false,
        }
    }
    fn tell(&mut self, player: u128, line: &str) -> bool {
        self.push(Later::Tell(player, line.to_owned()));
        true
    }
    fn broadcast(&mut self, line: &str) {
        self.push(Later::Broadcast(line.to_owned()));
    }
    fn run_command(&mut self, line: &str) -> bool {
        self.push(Later::Command(line.to_owned()));
        true
    }
    /// No `Host` to ask: only the level the caller compared counts.
    fn has_permission(&mut self, _player: u128, _node: &str, _default_level: u8) -> bool {
        false
    }
}

impl PluginHost {
    /// The plugins in `plugins/` beside `server.properties`, loaded the first time a hook asks.
    fn plugins(&self, host: &dyn Host) -> &Plugins {
        self.plugins.get_or_init(|| {
            let base = host
                .config_dir()
                .ok()
                .and_then(|c| c.parent().map(Path::to_path_buf))
                .unwrap_or_else(|| PathBuf::from("."));
            let plugins = Plugins::new(base.join("plugins"));
            for line in plugins.load_all() {
                log::info!("Plugin host: {line}");
            }
            plugins
        })
    }

    fn admin(&self, host: &mut dyn Host, line: &str) -> Flow {
        // `/plugin` and `/plugin list` list what `/plugins` lists, then the loadable .so plugins.
        if matches!(line.split_whitespace().nth(1), None | Some("list")) {
            plugins_list(host);
            let native = self.plugins(host).admin(line);
            if !native.starts_with("Plugins (0)") {
                host.feedback(Text::literal(native));
            }
            return Flow::Cancel;
        }
        let answer = self.plugins(host).admin(line);
        log::info!("Plugin host: {answer}");
        commands_changed(host, line);
        host.feedback(Text::literal(answer));
        Flow::Cancel
    }

    fn deferred<'a>(&'a self, replies: Option<&'a mut Vec<Text>>) -> Deferred<'a> {
        Deferred {
            later: &self.later,
            replies,
        }
    }
}

/// `/ver` and `/about`, with or without a plugin.
fn version_feedback(host: &mut dyn Host, line: &str) {
    let lines = version_reply(line).unwrap_or_else(|| {
        vec!["This server runs Horizon. /version shows the game version; /ver <plugin> shows a port.".into()]
    });
    for l in lines {
        host.feedback(Text::literal(l));
    }
}

fn taken(took: bool) -> Flow {
    if took { Flow::Cancel } else { Flow::Continue }
}

impl Hooks for PluginHost {
    fn register_commands(&self, host: &mut dyn Host, event: &mut RegisterCommandsEvent<'_>) {
        let plugins = self.plugins(host);
        for usage in PLUGIN_USAGES {
            event.commands.push(NativeCommand::op(usage, 4));
        }
        event.commands.push(NativeCommand::new("plugins"));
        event.commands.push(NativeCommand::new("pl"));
        // Vanilla owns `version`, which a mod cannot extend: `version <plugin>` is answered from
        // `player_command`/`console_line`; `ver` and `about` are ours and complete in the tree.
        for root in ["ver", "about"] {
            event
                .commands
                .push(NativeCommand::new(&format!("{root} [plugin:word]")));
        }
        let commands = plugins.commands();
        plugins.mark_registered(
            commands
                .iter()
                .map(|c| root(&c.usage).to_owned())
                .chain(["plugin", "plugins", "pl", "ver", "about"].map(str::to_owned)),
        );
        for c in commands {
            let command = NativeCommand::op(&c.usage, c.level);
            #[cfg(feature = "hooks-all")]
            let command = if c.node.is_empty() {
                command
            } else {
                command.permission(&c.node)
            };
            event.commands.push(command);
        }
    }

    fn perform_command(&self, host: &mut dyn Host, event: &mut PerformCommandEvent<'_>) -> Flow {
        match root(event.command) {
            "plugin" => return self.admin(host, event.command),
            "plugins" | "pl" => {
                plugins_list(host);
                return Flow::Cancel;
            }
            "ver" | "about" => {
                version_feedback(host, event.command);
                return Flow::Cancel;
            }
            _ => {}
        }
        let p = event.player;
        let sender = Sender {
            uuid: p.uuid,
            name: &p.name,
            permission: p.permission,
        };
        let plugins = self.plugins(host);
        let mut calls = HostCalls {
            host,
            sink: Sink::Feedback,
        };
        taken(plugins.command(&mut calls, Some(sender), event.command))
    }

    fn perform_console_command(
        &self,
        host: &mut dyn Host,
        event: &mut PerformConsoleCommandEvent<'_>,
    ) -> Flow {
        match root(event.command) {
            "plugin" => return self.admin(host, event.command),
            "plugins" | "pl" => {
                plugins_list(host);
                return Flow::Cancel;
            }
            "ver" | "about" => {
                version_feedback(host, event.command);
                return Flow::Cancel;
            }
            _ => {}
        }
        let plugins = self.plugins(host);
        let mut calls = HostCalls {
            host,
            sink: Sink::Feedback,
        };
        taken(plugins.command(&mut calls, None, event.command))
    }

    /// A command missing from the tree (a plugin loaded after start added it): runs at level 0
    /// only, since this hook does not know the player's level (a permission node still counts).
    fn player_command(&self, host: &mut dyn Host, event: &mut PlayerCommandEvent<'_>) -> Flow {
        if is_version_root(root(event.command)) {
            if let Some(lines) = version_reply(event.command) {
                event.replies.extend(lines.into_iter().map(Text::literal));
                return Flow::Cancel;
            }
        }
        let plugins = self.plugins(host);
        if plugins.is_registered(root(event.command)) {
            return Flow::Continue;
        }
        let sender = Sender {
            uuid: event.player,
            name: event.name,
            permission: 0,
        };
        let mut calls = HostCalls {
            host,
            sink: Sink::Replies(&mut *event.replies),
        };
        taken(plugins.command(&mut calls, Some(sender), event.command))
    }

    fn player_logged_in(&self, host: &mut dyn Host, event: &mut PlayerLoggedInEvent<'_>) {
        let permission = player_level(host, event.player);
        let plugins = self.plugins(host);
        let mut calls = HostCalls {
            host,
            sink: Sink::Log,
        };
        let player = Sender {
            uuid: event.player,
            name: event.name,
            permission,
        };
        plugins.player_joined(&mut calls, player);
    }

    /// Before the player is removed: helpers still reach them.
    #[cfg(feature = "hooks-all")]
    fn player_logging_out(&self, host: &mut dyn Host, event: &mut PlayerLoggingOutEvent<'_>) {
        let permission = player_level(host, event.player);
        let plugins = self.plugins(host);
        let mut calls = HostCalls {
            host,
            sink: Sink::Log,
        };
        let player = Sender {
            uuid: event.player,
            name: event.name,
            permission,
        };
        plugins.player_quit(&mut calls, player);
    }

    /// Upstream only: after the removal.
    #[cfg(not(feature = "hooks-all"))]
    fn player_logged_out(&self, host: &mut dyn Host, event: &mut PlayerLoggedOutEvent<'_>) {
        let plugins = self.plugins(host);
        let mut calls = HostCalls {
            host,
            sink: Sink::Log,
        };
        let player = Sender {
            uuid: event.player,
            name: event.name,
            permission: 0,
        };
        plugins.player_quit(&mut calls, player);
    }

    /// A rewrite cancels the signed line and broadcasts the new one as a system line on the next
    /// tick (there is no chat-decorate hook yet), so it reads `<name> text` without a signature.
    fn player_chat(&self, event: &mut PlayerChatEvent<'_>) -> Flow {
        let Some(plugins) = self.plugins.get() else {
            return Flow::Continue;
        };
        let player = Sender {
            uuid: event.player,
            name: event.name,
            permission: 0,
        };
        let mut calls = self.deferred(Some(&mut *event.replies));
        match plugins.chat(&mut calls, player, event.message) {
            Chat::Unchanged => Flow::Continue,
            Chat::Cancelled => Flow::Cancel,
            Chat::Rewritten(line) => {
                calls.broadcast(&format!("<{}> {line}", event.name));
                Flow::Cancel
            }
        }
    }

    fn server_tick(&self, host: &mut dyn Host, event: &mut ServerTickEvent<'_>) {
        if !matches!(event.phase, ServerTickPhase::ServerTickStart { .. }) {
            return;
        }
        let later = self
            .later
            .lock()
            .map(|mut l| std::mem::take(&mut *l))
            .unwrap_or_default();
        for item in later {
            match item {
                Later::Tell(player, line) => {
                    host.tell(player, Text::literal(line));
                }
                Later::Broadcast(line) => host.broadcast(Text::literal(line)),
                Later::Command(line) => {
                    host.run_command(&line);
                }
            }
        }
        let plugins = self.plugins(host);
        let mut calls = HostCalls {
            host,
            sink: Sink::Log,
        };
        plugins.tick(&mut calls);
    }

    /// The player is the acting one (`acting_player`), when there is one.
    fn destroy_block(&self, host: &mut dyn Host, event: &mut DestroyBlockEvent) -> Flow {
        let acting = host.acting_player();
        let plugins = self.plugins(host);
        let player = acting.as_ref().map(|p| Sender {
            uuid: p.uuid,
            name: &p.name,
            permission: p.permission,
        });
        let block = BlockAt {
            dimension: acting.as_ref().map_or("", |p| p.dimension.as_str()),
            pos: event.pos,
            state: event.state.0,
            item: "",
        };
        let mut calls = HostCalls {
            host,
            sink: Sink::Log,
        };
        taken(plugins.block_break(&mut calls, player, block))
    }

    #[cfg(feature = "hooks-all")]
    fn block_place(&self, host: &mut dyn Host, event: &mut BlockPlaceEvent<'_>) -> Flow {
        let acting = host.acting_player();
        let plugins = self.plugins(host);
        let player = Sender {
            uuid: event.player,
            name: event.name,
            permission: acting
                .as_ref()
                .filter(|p| p.uuid == event.player)
                .map_or(0, |p| p.permission),
        };
        let block = BlockAt {
            dimension: acting.as_ref().map_or("", |p| p.dimension.as_str()),
            pos: event.pos,
            state: event.state.0,
            item: &event.stack.item,
        };
        let mut calls = HostCalls {
            host,
            sink: Sink::Log,
        };
        taken(plugins.block_place(&mut calls, player, block))
    }

    #[cfg(feature = "hooks-all")]
    fn player_hurt_by_player(&self, event: &mut PlayerHurtByPlayerEvent<'_>) -> Flow {
        let Some(plugins) = self.plugins.get() else {
            return Flow::Continue;
        };
        let hit = horizon_plugin_loader::Hit {
            victim: event.victim,
            victim_entity: event.victim_entity,
            attacker: Some(event.attacker),
            attacker_entity: event.attacker_entity,
            source: event.source,
            damage_type: event.damage_type,
        };
        let mut amount = event.amount;
        let mut calls = self.deferred(None);
        let cancel = plugins.player_damage(&mut calls, hit, &mut amount);
        event.amount = amount;
        taken(cancel)
    }

    /// The `plugin` command itself is never cancelled here, so a plugin cannot lock admins out.
    #[cfg(feature = "hooks-all")]
    fn console_line(&self, host: &mut dyn Host, event: &mut ConsoleLineEvent<'_>) -> Flow {
        let line = event.line.trim_start_matches('/');
        if root(line) == "plugin" {
            return Flow::Continue;
        }
        if is_version_root(root(line)) {
            if let Some(lines) = version_reply(line) {
                event.replies.extend(lines.into_iter().map(Text::literal));
                return Flow::Cancel;
            }
        }
        let plugins = self.plugins(host);
        let mut calls = HostCalls {
            host,
            sink: Sink::Replies(&mut *event.replies),
        };
        taken(plugins.console_line(&mut calls, event.source, line))
    }

    fn server_stopping(&self, _host: &mut dyn Host, _event: &mut ServerStoppingEvent) {
        if let Some(plugins) = self.plugins.get() {
            plugins.unload_all();
        }
    }
}
