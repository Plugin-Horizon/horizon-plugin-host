//! The C ABI between a Horizon dedicated server's plugin host and server plugins built as
//! `cdylib`s.
//!
//! The host is an ordinary trait mod, built into the server once. A plugin is a shared library
//! in `plugins/` that exports [`ENTRY_SYMBOL`]. Nothing but the `#[repr(C)]` types below crosses
//! the boundary: no Rust type, no allocation, no panic. Strings are borrowed for one call.
//!
//! Versioning: the major version is in the symbol's name, so a plugin of another major is not
//! found at all. Within a major, fields are only ever appended to [`HostV1`] and [`PluginV1`],
//! and each side reads a field only when the other side's `size` covers it ([`HostV1::has`],
//! [`PluginV1::has`]).
//!
//! The second half of this file is the plugin side: implement [`ServerPlugin`] and call
//! [`export_plugin!`]; the generated functions catch every panic.
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// The major version this crate speaks.
pub const ABI_MAJOR: u32 = 1;
/// The symbol a plugin exports: `extern "C" fn() -> *mut PluginV1`, null when it failed to start.
pub const ENTRY_SYMBOL: &[u8] = b"horizon_server_plugin_v1\0";

/// A borrowed UTF-8 string, valid for the call it is passed to.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Str {
    pub ptr: *const u8,
    pub len: usize,
}

impl Str {
    pub const EMPTY: Str = Str {
        ptr: std::ptr::null(),
        len: 0,
    };

    pub fn new(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    /// The string, or "" when it is null or not UTF-8.
    ///
    /// # Safety
    /// `ptr` and `len` describe memory that stays readable for `'a`.
    pub unsafe fn as_str<'a>(self) -> &'a str {
        if self.ptr.is_null() {
            return "";
        }
        let bytes = unsafe { std::slice::from_raw_parts(self.ptr, self.len) };
        std::str::from_utf8(bytes).unwrap_or("")
    }
}

/// A player's UUID as two halves (u128 has no stable C layout). All zero means "none".
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Uuid {
    pub hi: u64,
    pub lo: u64,
}

impl From<u128> for Uuid {
    fn from(v: u128) -> Self {
        Self {
            hi: (v >> 64) as u64,
            lo: v as u64,
        }
    }
}

impl From<Uuid> for u128 {
    fn from(v: Uuid) -> Self {
        ((v.hi as u128) << 64) | v.lo as u128
    }
}

/// A player a hook is about.
#[repr(C)]
pub struct Player {
    pub uuid: Uuid,
    pub name: Str,
    /// Operator level, 0 to 4, as far as the host knows it (0 when it does not).
    pub permission: u8,
}

/// A block a hook is about.
#[repr(C)]
pub struct Block {
    /// `minecraft:overworld`, or "" when the hook does not say.
    pub dimension: Str,
    pub x: i32,
    pub y: i32,
    pub z: i32,
    /// The game's block state id (`BlockStateId`), for the world it is in.
    pub state: u32,
    /// The item placed (`minecraft:oak_planks`), or "" for a break.
    pub item: Str,
}

/// One player hurting another.
#[repr(C)]
pub struct Damage {
    pub victim: Uuid,
    pub victim_entity: i32,
    /// All zero when no player caused it.
    pub attacker: Uuid,
    pub attacker_entity: i32,
    /// `melee`, `sweep`, or a projectile's entity type.
    pub source: Str,
    /// `minecraft:player_attack`, `minecraft:arrow`, ...
    pub damage_type: Str,
}

/// What a hook that may stop the game's action answers.
pub type Verdict = u8;
/// Let the action go on.
pub const CONTINUE: Verdict = 0;
/// Stop it (the chat line is not sent, the block is not broken, the hit does nothing...).
pub const CANCEL: Verdict = 1;

/// One hook call's context inside the host; only passed back to [`HostV1`]'s functions during
/// that call.
#[repr(C)]
pub struct Call {
    _private: [u8; 0],
}

/// What the host offers a plugin, during a hook call. Fields after `run_command` are newer:
/// check [`HostV1::has`] before calling one.
#[repr(C)]
pub struct HostV1 {
    /// `size_of::<HostV1>()` in the host's build.
    pub size: usize,
    pub major: u32,
    /// A line in the server's log, named after the plugin.
    pub log: extern "C" fn(call: *mut Call, line: Str),
    /// A line of the answer to the command being handled (the player's chat, or the console).
    pub reply: extern "C" fn(call: *mut Call, line: Str) -> bool,
    /// A system chat line for a player; false when they are not on.
    pub tell: extern "C" fn(call: *mut Call, player: Uuid, line: Str) -> bool,
    /// A system chat line for every player.
    pub broadcast: extern "C" fn(call: *mut Call, line: Str),
    /// A command as the server's console; false where the game does not allow it.
    pub run_command: extern "C" fn(call: *mut Call, line: Str) -> bool,
    /// Whether `player` has the permission `node`; a host without a permissions provider
    /// answers `operator level >= default_level`.
    pub has_permission:
        extern "C" fn(call: *mut Call, player: Uuid, node: Str, default_level: u8) -> bool,
    /// Runs the plugin's `task` with `id` after `delay` ticks, then every `period` ticks (0: once).
    /// Returns a handle for `cancel_task`, 0 when it was refused. Tasks end with the plugin.
    pub schedule: extern "C" fn(call: *mut Call, id: u64, delay: u64, period: u64) -> u64,
    /// Stops a task `schedule` returned; false when it was not the plugin's or ran out.
    pub cancel_task: extern "C" fn(call: *mut Call, handle: u64) -> bool,
    /// The plugin's own folder, `plugins/<name>/`, created before it starts.
    pub config_dir: extern "C" fn(call: *mut Call) -> Str,
    /// During a `chat` call only: the line the next plugins and the players see. False elsewhere.
    pub rewrite_chat: extern "C" fn(call: *mut Call, line: Str) -> bool,
}

impl HostV1 {
    /// True when this host's table reaches the field that ends at `end` (`offset + size`).
    pub fn has(&self, end: usize) -> bool {
        self.size >= end
    }
}

/// A loaded plugin, made by its [`ENTRY_SYMBOL`] and owned by the plugin: the host gives it back
/// through `unload` and never frees it itself. Fields after `unload` are newer: the host checks
/// [`PluginV1::has`] before calling one.
#[repr(C)]
pub struct PluginV1 {
    /// `size_of::<PluginV1>()` in the plugin's build.
    pub size: usize,
    pub major: u32,
    /// Static strings of the plugin.
    pub name: Str,
    pub version: Str,
    pub state: *mut c_void,
    /// Calls `add` once per command usage the plugin answers (`hello [name:word]`), with the
    /// operator level it needs and a permission node ("" for none) that grants it too.
    pub commands: extern "C" fn(
        state: *mut c_void,
        sink: *mut c_void,
        add: extern "C" fn(sink: *mut c_void, usage: Str, level: u8, node: Str),
    ),
    /// A command line without its slash; `sender` is null for the console. True when it was the
    /// plugin's.
    pub command: extern "C" fn(
        state: *mut c_void,
        host: *const HostV1,
        call: *mut Call,
        sender: *const Player,
        line: Str,
    ) -> bool,
    /// A player joined the server.
    pub player_joined: extern "C" fn(
        state: *mut c_void,
        host: *const HostV1,
        call: *mut Call,
        player: *const Player,
    ),
    /// Drops the plugin's state and this struct. No call follows.
    pub unload: extern "C" fn(plugin: *mut PluginV1),
    /// A player is leaving the server (still listed).
    pub player_quit: extern "C" fn(
        state: *mut c_void,
        host: *const HostV1,
        call: *mut Call,
        player: *const Player,
    ),
    /// A chat line, already rewritten by the plugins before; `rewrite_chat` changes it.
    pub chat: extern "C" fn(
        state: *mut c_void,
        host: *const HostV1,
        call: *mut Call,
        player: *const Player,
        message: Str,
    ) -> Verdict,
    /// The start of a server tick; `tick` counts from the host's start.
    pub tick: extern "C" fn(state: *mut c_void, host: *const HostV1, call: *mut Call, tick: u64),
    /// A task `schedule` set is due.
    pub task: extern "C" fn(state: *mut c_void, host: *const HostV1, call: *mut Call, id: u64),
    /// A block is about to be broken; `player` is null when the host does not know who.
    pub block_break: extern "C" fn(
        state: *mut c_void,
        host: *const HostV1,
        call: *mut Call,
        player: *const Player,
        block: *const Block,
    ) -> Verdict,
    /// A player is about to place a block.
    pub block_place: extern "C" fn(
        state: *mut c_void,
        host: *const HostV1,
        call: *mut Call,
        player: *const Player,
        block: *const Block,
    ) -> Verdict,
    /// A player is about to be hurt; `amount` may be changed.
    pub player_damage: extern "C" fn(
        state: *mut c_void,
        host: *const HostV1,
        call: *mut Call,
        damage: *const Damage,
        amount: *mut f32,
    ) -> Verdict,
    /// A console or RCON line (`source` is `console` or `rcon`) before any command sees it.
    pub console_line: extern "C" fn(
        state: *mut c_void,
        host: *const HostV1,
        call: *mut Call,
        source: Str,
        line: Str,
    ) -> Verdict,
}

impl PluginV1 {
    /// True when this plugin's table reaches the field that ends at `end` (`offset + size`).
    pub fn has(&self, end: usize) -> bool {
        self.size >= end
    }
}

/// The end of a field, for [`HostV1::has`] and [`PluginV1::has`]:
/// `field_end!(PluginV1, console_line)`.
#[macro_export]
macro_rules! field_end {
    ($ty:ty, $field:ident) => {
        ::std::mem::offset_of!($ty, $field) + ::std::mem::size_of::<usize>()
    };
}

/// The smallest `PluginV1` a host of this major accepts: the prototype's fields, up to `unload`.
pub const PLUGIN_V1_MIN_SIZE: usize = field_end!(PluginV1, unload);
/// The smallest `HostV1` a plugin of this major may meet: up to `run_command`.
pub const HOST_V1_MIN_SIZE: usize = field_end!(HostV1, run_command);

/// The entry point's type.
pub type EntryV1 = unsafe extern "C" fn() -> *mut PluginV1;

// ---- Plugin side --------------------------------------------------------------------------

/// A player, as a plugin sees one.
pub struct PlayerInfo<'a> {
    pub uuid: u128,
    pub name: &'a str,
    pub permission: u8,
}

impl PlayerInfo<'_> {
    /// # Safety
    /// `p` comes from the host, for the current call.
    unsafe fn from_raw(p: &Player) -> Self {
        Self {
            uuid: p.uuid.into(),
            name: unsafe { p.name.as_str() },
            permission: p.permission,
        }
    }
}

/// A block, as a plugin sees one.
pub struct BlockInfo<'a> {
    pub dimension: &'a str,
    pub pos: [i32; 3],
    pub state: u32,
    pub item: &'a str,
}

/// A hit between players, as a plugin sees one.
pub struct DamageInfo<'a> {
    pub victim: u128,
    pub victim_entity: i32,
    pub attacker: Option<u128>,
    pub attacker_entity: i32,
    pub source: &'a str,
    pub damage_type: &'a str,
}

/// What a cancellable hook answers, on the plugin side.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flow {
    Continue,
    Cancel,
}

impl Flow {
    fn raw(self) -> Verdict {
        match self {
            Flow::Continue => CONTINUE,
            Flow::Cancel => CANCEL,
        }
    }
}

/// The host, during one hook call.
pub struct Cx<'a> {
    host: &'a HostV1,
    call: *mut Call,
}

macro_rules! newer {
    ($self:ident, $field:ident, $missing:expr) => {
        if !$self.host.has(field_end!(HostV1, $field)) {
            return $missing;
        }
    };
}

impl Cx<'_> {
    pub fn log(&mut self, line: &str) {
        (self.host.log)(self.call, Str::new(line))
    }
    pub fn reply(&mut self, line: &str) -> bool {
        (self.host.reply)(self.call, Str::new(line))
    }
    pub fn tell(&mut self, player: u128, line: &str) -> bool {
        (self.host.tell)(self.call, player.into(), Str::new(line))
    }
    pub fn broadcast(&mut self, line: &str) {
        (self.host.broadcast)(self.call, Str::new(line))
    }
    pub fn run_command(&mut self, line: &str) -> bool {
        (self.host.run_command)(self.call, Str::new(line))
    }
    /// `None` when the host is older than this function.
    pub fn has_permission(&mut self, player: u128, node: &str, default_level: u8) -> Option<bool> {
        newer!(self, has_permission, None);
        Some((self.host.has_permission)(
            self.call,
            player.into(),
            Str::new(node),
            default_level,
        ))
    }
    /// Runs [`ServerPlugin::task`] with `id` after `delay` ticks, then every `period` ticks
    /// (0: once). `None` when refused or the host is older.
    pub fn schedule(&mut self, id: u64, delay: u64, period: u64) -> Option<u64> {
        newer!(self, schedule, None);
        match (self.host.schedule)(self.call, id, delay, period) {
            0 => None,
            handle => Some(handle),
        }
    }
    pub fn cancel_task(&mut self, handle: u64) -> bool {
        newer!(self, cancel_task, false);
        (self.host.cancel_task)(self.call, handle)
    }
    /// `plugins/<name>/`; `None` when the host is older.
    pub fn config_dir(&mut self) -> Option<std::path::PathBuf> {
        newer!(self, config_dir, None);
        let dir = unsafe { (self.host.config_dir)(self.call).as_str() };
        (!dir.is_empty()).then(|| std::path::PathBuf::from(dir))
    }
    /// In [`ServerPlugin::chat`] only: the line everyone sees instead.
    pub fn rewrite_chat(&mut self, line: &str) -> bool {
        newer!(self, rewrite_chat, false);
        (self.host.rewrite_chat)(self.call, Str::new(line))
    }
}

/// A command a plugin answers: its usage, the operator level it needs, and a permission node that
/// grants it as well (the host's permissions provider decides).
#[derive(Clone, Copy)]
pub struct CommandSpec {
    pub usage: &'static str,
    pub level: u8,
    pub node: &'static str,
}

impl CommandSpec {
    pub const fn new(usage: &'static str, level: u8) -> Self {
        Self {
            usage,
            level,
            node: "",
        }
    }
    pub const fn node(mut self, node: &'static str) -> Self {
        self.node = node;
        self
    }
}

/// A server plugin. Its state lives until the host unloads it (`Drop` runs then); state that must
/// survive a reload goes to a file in [`Cx::config_dir`].
pub trait ServerPlugin: Send + Sync + 'static {
    const NAME: &'static str;
    const VERSION: &'static str;
    fn commands(&self) -> Vec<CommandSpec> {
        Vec::new()
    }
    /// True when `line` was this plugin's command.
    fn command(&self, _cx: &mut Cx<'_>, _sender: Option<&PlayerInfo<'_>>, _line: &str) -> bool {
        false
    }
    fn player_joined(&self, _cx: &mut Cx<'_>, _player: &PlayerInfo<'_>) {}
    fn player_quit(&self, _cx: &mut Cx<'_>, _player: &PlayerInfo<'_>) {}
    fn chat(&self, _cx: &mut Cx<'_>, _player: &PlayerInfo<'_>, _message: &str) -> Flow {
        Flow::Continue
    }
    fn tick(&self, _cx: &mut Cx<'_>, _tick: u64) {}
    fn task(&self, _cx: &mut Cx<'_>, _id: u64) {}
    fn block_break(
        &self,
        _cx: &mut Cx<'_>,
        _player: Option<&PlayerInfo<'_>>,
        _block: &BlockInfo<'_>,
    ) -> Flow {
        Flow::Continue
    }
    fn block_place(
        &self,
        _cx: &mut Cx<'_>,
        _player: &PlayerInfo<'_>,
        _block: &BlockInfo<'_>,
    ) -> Flow {
        Flow::Continue
    }
    fn player_damage(&self, _cx: &mut Cx<'_>, _damage: &DamageInfo<'_>, _amount: &mut f32) -> Flow {
        Flow::Continue
    }
    fn console_line(&self, _cx: &mut Cx<'_>, _source: &str, _line: &str) -> Flow {
        Flow::Continue
    }
}

/// Builds the [`PluginV1`] for `P`; what [`export_plugin!`] calls.
pub fn export<P: ServerPlugin>(make: impl FnOnce() -> P) -> *mut PluginV1 {
    let Ok(plugin) = catch_unwind(AssertUnwindSafe(make)) else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(PluginV1 {
        size: size_of::<PluginV1>(),
        major: ABI_MAJOR,
        name: Str::new(P::NAME),
        version: Str::new(P::VERSION),
        state: Box::into_raw(Box::new(plugin)).cast(),
        commands: commands::<P>,
        command: command::<P>,
        player_joined: player_joined::<P>,
        unload: unload::<P>,
        player_quit: player_quit::<P>,
        chat: chat::<P>,
        tick: tick::<P>,
        task: task::<P>,
        block_break: block_break::<P>,
        block_place: block_place::<P>,
        player_damage: player_damage::<P>,
        console_line: console_line::<P>,
    }))
}

/// Runs `f` with the plugin and a `Cx`, catching a panic (then `missing`).
fn guarded<P: ServerPlugin, R>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    missing: R,
    f: impl FnOnce(&P, &mut Cx<'_>) -> R,
) -> R {
    catch_unwind(AssertUnwindSafe(|| {
        let plugin = unsafe { &*(state as *const P) };
        let mut cx = Cx {
            host: unsafe { &*host },
            call,
        };
        f(plugin, &mut cx)
    }))
    .unwrap_or(missing)
}

extern "C" fn commands<P: ServerPlugin>(
    state: *mut c_void,
    sink: *mut c_void,
    add: extern "C" fn(*mut c_void, Str, u8, Str),
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let plugin = unsafe { &*(state as *const P) };
        for c in plugin.commands() {
            add(sink, Str::new(c.usage), c.level, Str::new(c.node));
        }
    }));
}

extern "C" fn command<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    sender: *const Player,
    line: Str,
) -> bool {
    guarded::<P, _>(state, host, call, false, |p, cx| {
        let sender = unsafe { sender.as_ref() }.map(|s| unsafe { PlayerInfo::from_raw(s) });
        p.command(cx, sender.as_ref(), unsafe { line.as_str() })
    })
}

extern "C" fn player_joined<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    player: *const Player,
) {
    guarded::<P, _>(state, host, call, (), |p, cx| {
        if let Some(pl) = unsafe { player.as_ref() } {
            p.player_joined(cx, &unsafe { PlayerInfo::from_raw(pl) });
        }
    })
}

extern "C" fn player_quit<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    player: *const Player,
) {
    guarded::<P, _>(state, host, call, (), |p, cx| {
        if let Some(pl) = unsafe { player.as_ref() } {
            p.player_quit(cx, &unsafe { PlayerInfo::from_raw(pl) });
        }
    })
}

extern "C" fn chat<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    player: *const Player,
    message: Str,
) -> Verdict {
    guarded::<P, _>(state, host, call, CONTINUE, |p, cx| {
        let Some(pl) = (unsafe { player.as_ref() }) else {
            return CONTINUE;
        };
        p.chat(cx, &unsafe { PlayerInfo::from_raw(pl) }, unsafe {
            message.as_str()
        })
        .raw()
    })
}

extern "C" fn tick<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    n: u64,
) {
    guarded::<P, _>(state, host, call, (), |p, cx| p.tick(cx, n))
}

extern "C" fn task<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    id: u64,
) {
    guarded::<P, _>(state, host, call, (), |p, cx| p.task(cx, id))
}

unsafe fn block_info<'a>(b: &'a Block) -> BlockInfo<'a> {
    BlockInfo {
        dimension: unsafe { b.dimension.as_str() },
        pos: [b.x, b.y, b.z],
        state: b.state,
        item: unsafe { b.item.as_str() },
    }
}

extern "C" fn block_break<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    player: *const Player,
    block: *const Block,
) -> Verdict {
    guarded::<P, _>(state, host, call, CONTINUE, |p, cx| {
        let Some(b) = (unsafe { block.as_ref() }) else {
            return CONTINUE;
        };
        let pl = unsafe { player.as_ref() }.map(|pl| unsafe { PlayerInfo::from_raw(pl) });
        p.block_break(cx, pl.as_ref(), &unsafe { block_info(b) })
            .raw()
    })
}

extern "C" fn block_place<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    player: *const Player,
    block: *const Block,
) -> Verdict {
    guarded::<P, _>(state, host, call, CONTINUE, |p, cx| {
        let (Some(pl), Some(b)) = (unsafe { player.as_ref() }, unsafe { block.as_ref() }) else {
            return CONTINUE;
        };
        p.block_place(cx, &unsafe { PlayerInfo::from_raw(pl) }, &unsafe {
            block_info(b)
        })
        .raw()
    })
}

extern "C" fn player_damage<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    damage: *const Damage,
    amount: *mut f32,
) -> Verdict {
    guarded::<P, _>(state, host, call, CONTINUE, |p, cx| {
        let (Some(d), Some(amount)) = (unsafe { damage.as_ref() }, unsafe { amount.as_mut() })
        else {
            return CONTINUE;
        };
        let attacker: u128 = d.attacker.into();
        let info = DamageInfo {
            victim: d.victim.into(),
            victim_entity: d.victim_entity,
            attacker: (attacker != 0).then_some(attacker),
            attacker_entity: d.attacker_entity,
            source: unsafe { d.source.as_str() },
            damage_type: unsafe { d.damage_type.as_str() },
        };
        p.player_damage(cx, &info, amount).raw()
    })
}

extern "C" fn console_line<P: ServerPlugin>(
    state: *mut c_void,
    host: *const HostV1,
    call: *mut Call,
    source: Str,
    line: Str,
) -> Verdict {
    guarded::<P, _>(state, host, call, CONTINUE, |p, cx| {
        p.console_line(cx, unsafe { source.as_str() }, unsafe { line.as_str() })
            .raw()
    })
}

extern "C" fn unload<P: ServerPlugin>(plugin: *mut PluginV1) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let plugin = unsafe { Box::from_raw(plugin) };
        drop(unsafe { Box::from_raw(plugin.state as *mut P) });
    }));
}

/// Exports `$ty`, made by `$make`, as this library's plugin.
#[macro_export]
macro_rules! export_plugin {
    ($ty:ty, $make:expr) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn horizon_server_plugin_v1() -> *mut $crate::PluginV1 {
            $crate::export::<$ty>(|| $make)
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prototype_fields_come_first_and_newer_ones_are_size_checked() {
        // A plugin built against the prototype (fields up to `unload`) is still accepted.
        assert_eq!(
            PLUGIN_V1_MIN_SIZE,
            std::mem::offset_of!(PluginV1, unload) + size_of::<usize>()
        );
        assert!(PLUGIN_V1_MIN_SIZE < size_of::<PluginV1>());
        assert_eq!(field_end!(PluginV1, console_line), size_of::<PluginV1>());
        assert_eq!(field_end!(HostV1, rewrite_chat), size_of::<HostV1>());
        let old = PluginV1 {
            size: PLUGIN_V1_MIN_SIZE,
            ..unsafe { std::ptr::read(export::<Nothing>(|| Nothing)) }
        };
        assert!(old.has(field_end!(PluginV1, unload)));
        assert!(!old.has(field_end!(PluginV1, player_quit)));
    }

    #[test]
    fn uuids_round_trip() {
        let v: u128 = 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210;
        assert_eq!(u128::from(Uuid::from(v)), v);
    }

    struct Nothing;
    impl ServerPlugin for Nothing {
        const NAME: &'static str = "nothing";
        const VERSION: &'static str = "0";
    }
}
