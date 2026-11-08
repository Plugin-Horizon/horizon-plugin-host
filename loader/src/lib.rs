//! Loads server plugins (`cdylib`s speaking `horizon-plugin-abi`) from a folder and reloads them
//! while the server runs. Engine-free: the plugin host (a trait mod) hands it a [`Calls`] for each
//! hook call, so this crate is tested without a server (tests/reload.rs).
//!
//! A reload drops the old plugin's state and tasks, stops calling it and loads the new file. The
//! old library stays mapped until the process exits: unmapping Rust code is not sound (a thread,
//! a thread-local's destructor or a `'static` it handed out would point into freed memory), so
//! each reload costs the size of one copy of the library.
//!
//! Cancellable events go to the plugins in load order; the first that cancels ends the event and
//! the later ones do not hear it.
use horizon_plugin_abi::{self as abi, field_end};
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

/// What the game does for a plugin during one hook call.
pub trait Calls {
    fn log(&mut self, plugin: &str, line: &str);
    /// A line of the answer to the command being handled.
    fn reply(&mut self, plugin: &str, line: &str) -> bool;
    fn tell(&mut self, player: u128, line: &str) -> bool;
    fn broadcast(&mut self, line: &str);
    fn run_command(&mut self, line: &str) -> bool;
    /// Whether `player` has `node`; without a permissions provider, whether their operator level
    /// reaches `default_level`.
    fn has_permission(&mut self, player: u128, node: &str, default_level: u8) -> bool;
}

/// A player a hook is about; `permission` is the operator level as far as the host knows it.
#[derive(Clone, Copy)]
pub struct Sender<'a> {
    pub uuid: u128,
    pub name: &'a str,
    pub permission: u8,
}

/// A block a hook is about.
#[derive(Clone, Copy)]
pub struct BlockAt<'a> {
    pub dimension: &'a str,
    pub pos: [i32; 3],
    pub state: u32,
    /// The item placed; "" for a break.
    pub item: &'a str,
}

/// One player hurting another.
#[derive(Clone, Copy)]
pub struct Hit<'a> {
    pub victim: u128,
    pub victim_entity: i32,
    pub attacker: Option<u128>,
    pub attacker_entity: i32,
    pub source: &'a str,
    pub damage_type: &'a str,
}

/// A chat line after the plugins.
#[derive(Debug, PartialEq, Eq)]
pub enum Chat {
    /// Send it as it was.
    Unchanged,
    /// Send this instead.
    Rewritten(String),
    /// Send nothing.
    Cancelled,
}

/// A command a plugin answers.
#[derive(Clone, Debug)]
pub struct PluginCommand {
    pub usage: String,
    /// The operator level that may run it.
    pub level: u8,
    /// A permission node that grants it as well; "" for none.
    pub node: String,
}

pub struct Plugins {
    dir: PathBuf,
    list: RwLock<Vec<Loaded>>,
    /// Command roots in the players' command tree, which the game builds once.
    registered: Mutex<Vec<String>>,
    loads: AtomicU32,
    /// Server ticks seen, for tasks.
    ticks: AtomicU64,
    handles: AtomicU64,
}

struct Task {
    handle: u64,
    id: u64,
    due: u64,
    period: u64,
}

struct Loaded {
    name: String,
    version: String,
    load: u32,
    commands: Vec<PluginCommand>,
    config_dir: String,
    tasks: Mutex<Vec<Task>>,
    plugin: *mut abi::PluginV1,
    /// Never dropped: see the crate comment.
    _library: ManuallyDrop<libloading::Library>,
}

// The plugin's state is `Send + Sync` by the SDK's `ServerPlugin` bound; the pointer is only
// passed back to its own functions.
unsafe impl Send for Loaded {}
unsafe impl Sync for Loaded {}

struct CallCx<'a> {
    calls: &'a mut dyn Calls,
    plugins: &'a Plugins,
    plugin: &'a str,
    config_dir: &'a str,
    tasks: &'a Mutex<Vec<Task>>,
    /// The line being chatted, during a chat call.
    chat: Option<&'a mut String>,
}

static HOST: abi::HostV1 = abi::HostV1 {
    size: size_of::<abi::HostV1>(),
    major: abi::ABI_MAJOR,
    log: host_log,
    reply: host_reply,
    tell: host_tell,
    broadcast: host_broadcast,
    run_command: host_run_command,
    has_permission: host_has_permission,
    schedule: host_schedule,
    cancel_task: host_cancel_task,
    config_dir: host_config_dir,
    rewrite_chat: host_rewrite_chat,
};

/// # Safety
/// `call` is the `CallCx` of the hook call in progress.
unsafe fn cx<'x>(call: *mut abi::Call) -> &'x mut CallCx<'x> {
    unsafe { &mut *(call as *mut CallCx<'x>) }
}

extern "C" fn host_log(call: *mut abi::Call, line: abi::Str) {
    let cx = unsafe { cx(call) };
    cx.calls.log(cx.plugin, unsafe { line.as_str() });
}

extern "C" fn host_reply(call: *mut abi::Call, line: abi::Str) -> bool {
    let cx = unsafe { cx(call) };
    cx.calls.reply(cx.plugin, unsafe { line.as_str() })
}

extern "C" fn host_tell(call: *mut abi::Call, player: abi::Uuid, line: abi::Str) -> bool {
    unsafe { cx(call) }
        .calls
        .tell(player.into(), unsafe { line.as_str() })
}

extern "C" fn host_broadcast(call: *mut abi::Call, line: abi::Str) {
    unsafe { cx(call) }
        .calls
        .broadcast(unsafe { line.as_str() })
}

extern "C" fn host_run_command(call: *mut abi::Call, line: abi::Str) -> bool {
    unsafe { cx(call) }
        .calls
        .run_command(unsafe { line.as_str() })
}

extern "C" fn host_has_permission(
    call: *mut abi::Call,
    player: abi::Uuid,
    node: abi::Str,
    default_level: u8,
) -> bool {
    unsafe { cx(call) }
        .calls
        .has_permission(player.into(), unsafe { node.as_str() }, default_level)
}

extern "C" fn host_schedule(call: *mut abi::Call, id: u64, delay: u64, period: u64) -> u64 {
    let cx = unsafe { cx(call) };
    let Ok(mut tasks) = cx.tasks.lock() else {
        return 0;
    };
    let handle = cx.plugins.handles.fetch_add(1, Ordering::Relaxed) + 1;
    tasks.push(Task {
        handle,
        id,
        due: cx.plugins.ticks.load(Ordering::Relaxed) + delay.max(1),
        period,
    });
    handle
}

extern "C" fn host_cancel_task(call: *mut abi::Call, handle: u64) -> bool {
    let cx = unsafe { cx(call) };
    let Ok(mut tasks) = cx.tasks.lock() else {
        return false;
    };
    let before = tasks.len();
    tasks.retain(|t| t.handle != handle);
    tasks.len() != before
}

extern "C" fn host_config_dir(call: *mut abi::Call) -> abi::Str {
    abi::Str::new(unsafe { cx(call) }.config_dir)
}

extern "C" fn host_rewrite_chat(call: *mut abi::Call, line: abi::Str) -> bool {
    let cx = unsafe { cx(call) };
    match cx.chat.as_deref_mut() {
        Some(message) => {
            *message = unsafe { line.as_str() }.to_owned();
            true
        }
        None => false,
    }
}

extern "C" fn add_command(sink: *mut std::ffi::c_void, usage: abi::Str, level: u8, node: abi::Str) {
    let list = unsafe { &mut *(sink as *mut Vec<PluginCommand>) };
    list.push(PluginCommand {
        usage: unsafe { usage.as_str() }.to_owned(),
        level: level.min(4),
        node: unsafe { node.as_str() }.to_owned(),
    });
}

/// A command line's first word.
pub fn root(line: &str) -> &str {
    line.split(' ').next().unwrap_or_default()
}

fn raw(sender: Sender<'_>) -> abi::Player {
    abi::Player {
        uuid: sender.uuid.into(),
        name: abi::Str::new(sender.name),
        permission: sender.permission,
    }
}

fn raw_block(b: BlockAt<'_>) -> abi::Block {
    abi::Block {
        dimension: abi::Str::new(b.dimension),
        x: b.pos[0],
        y: b.pos[1],
        z: b.pos[2],
        state: b.state,
        item: abi::Str::new(b.item),
    }
}

fn player_ptr(p: &Option<abi::Player>) -> *const abi::Player {
    p.as_ref()
        .map_or(std::ptr::null(), |p| p as *const abi::Player)
}

impl Loaded {
    fn plugin(&self) -> &abi::PluginV1 {
        unsafe { &*self.plugin }
    }

    fn unload(self) {
        (self.plugin().unload)(self.plugin);
        // `_library` is ManuallyDrop: the code stays mapped. The tasks go with `self`.
    }
}

impl Plugins {
    /// Plugins in `dir` (`<name>.so`), none loaded yet.
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(dir.join(".loaded"));
        Self {
            dir,
            list: RwLock::default(),
            registered: Mutex::default(),
            loads: AtomicU32::new(0),
            ticks: AtomicU64::new(0),
            handles: AtomicU64::new(0),
        }
    }

    /// Loads every plugin file in the folder; one line per file.
    pub fn load_all(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                name.strip_suffix(std::env::consts::DLL_SUFFIX)
                    .map(str::to_owned)
            })
            .collect();
        names.sort();
        let mut list = self.list.write().unwrap();
        names
            .into_iter()
            .map(|name| match self.open(&name) {
                Ok(p) => {
                    let line = format!("loaded {} {}", p.name, p.version);
                    list.push(p);
                    line
                }
                Err(e) => format!("{name} not loaded: {e}"),
            })
            .collect()
    }

    /// Every loaded plugin's commands.
    pub fn commands(&self) -> Vec<PluginCommand> {
        let list = self.list.read().unwrap();
        list.iter()
            .flat_map(|p| p.commands.iter().cloned())
            .collect()
    }

    /// Notes that the game's command tree has these roots.
    pub fn mark_registered(&self, roots: impl IntoIterator<Item = String>) {
        self.registered.lock().unwrap().extend(roots);
    }

    pub fn is_registered(&self, root: &str) -> bool {
        self.registered.lock().unwrap().iter().any(|r| r == root)
    }

    /// Maps `<dir>/<name>.so` through a copy of its own, so the file can be rebuilt in place
    /// while the copy is mapped, and a second load is a new library, not the first one again
    /// (the dynamic loader hands out the library already loaded from a path). Runs none of the
    /// plugin's code but its library's initializers; a plugin of another ABI fails here.
    fn map(&self, name: &str) -> Result<(libloading::Library, abi::EntryV1, u32), String> {
        if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) {
            return Err(format!("{name:?} is not a plugin name"));
        }
        let file = self
            .dir
            .join(format!("{name}{}", std::env::consts::DLL_SUFFIX));
        if !file.is_file() {
            // The server's own path stays out of the chat: the folder's name is enough.
            return Err(format!(
                "no {name}{} in plugins/",
                std::env::consts::DLL_SUFFIX
            ));
        }
        let load = self.loads.fetch_add(1, Ordering::Relaxed) + 1;
        let copy = self
            .dir
            .join(".loaded")
            .join(format!("{name}-{load}{}", std::env::consts::DLL_SUFFIX));
        std::fs::copy(&file, &copy).map_err(|e| format!("copy: {e}"))?;
        let library = unsafe { libloading::Library::new(&copy) };
        // The mapping keeps the copy's contents; the name is not needed any more.
        #[cfg(unix)]
        let _ = std::fs::remove_file(&copy);
        let library = library.map_err(|e| format!("dlopen: {e}"))?;
        let entry = unsafe { library.get::<abi::EntryV1>(abi::ENTRY_SYMBOL).map(|s| *s) };
        match entry {
            Ok(entry) => Ok((library, entry, load)),
            Err(_) => {
                // Never unmapped either: its initializers ran.
                std::mem::forget(library);
                Err("no horizon_server_plugin_v1: not a plugin of ABI 1".into())
            }
        }
    }

    fn open(&self, name: &str) -> Result<Loaded, String> {
        let mapped = self.map(name)?;
        self.start(name, mapped)
    }

    /// Runs the plugin's entry point, after making its folder `plugins/<name>/`.
    fn start(
        &self,
        name: &str,
        (library, entry, load): (libloading::Library, abi::EntryV1, u32),
    ) -> Result<Loaded, String> {
        let library = ManuallyDrop::new(library);
        let config_dir = self.dir.join(name);
        std::fs::create_dir_all(&config_dir).map_err(|e| format!("config folder: {e}"))?;
        let plugin = unsafe { entry() };
        if plugin.is_null() {
            return Err("its entry point failed".into());
        }
        let p = unsafe { &*plugin };
        if p.major != abi::ABI_MAJOR || p.size < abi::PLUGIN_V1_MIN_SIZE {
            // Another ABI's struct: nothing else of it is called.
            return Err(format!(
                "built for plugin ABI {} (size {}), this host speaks {} (size {} or more)",
                p.major,
                p.size,
                abi::ABI_MAJOR,
                abi::PLUGIN_V1_MIN_SIZE
            ));
        }
        let mut commands = Vec::new();
        (p.commands)(
            p.state,
            (&mut commands as *mut Vec<PluginCommand>).cast(),
            add_command,
        );
        let loaded = Loaded {
            name: unsafe { p.name.as_str() }.to_owned(),
            version: unsafe { p.version.as_str() }.to_owned(),
            load,
            commands,
            config_dir: config_dir.to_string_lossy().into_owned(),
            tasks: Mutex::default(),
            plugin,
            _library: library,
        };
        if loaded.name != name {
            let declared = loaded.name.clone();
            loaded.unload();
            return Err(format!("the file {name} holds plugin {declared}"));
        }
        Ok(loaded)
    }

    /// `plugin <list|load|unload|reload> [name]`, the answer as one line.
    pub fn admin(&self, line: &str) -> String {
        let mut words = line.split_whitespace().skip(1);
        let (action, name) = (words.next().unwrap_or("list"), words.next());
        // A plugin's own command may run `/plugin`: the list is borrowed then.
        let Ok(mut list) = self.list.try_write() else {
            return "Plugins are busy; try again".into();
        };
        let at = name.and_then(|n| list.iter().position(|p| p.name == n));
        match (action, name) {
            ("list", _) => {
                let names: Vec<_> = list
                    .iter()
                    .map(|p| format!("{} {} (load #{})", p.name, p.version, p.load))
                    .collect();
                format!("Plugins ({}): {}", names.len(), names.join(", "))
            }
            ("unload", Some(n)) => match at {
                Some(i) => {
                    list.remove(i).unload();
                    format!("Unloaded {n}")
                }
                None => format!("{n} is not loaded"),
            },
            ("load" | "reload", Some(n)) => {
                if action == "load" && at.is_some() {
                    return format!("{n} is loaded already; use /plugin reload {n}");
                }
                let started = Instant::now();
                // The new file is mapped first: one that is not a plugin of this ABI leaves the old
                // one running.
                let mapped = match self.map(n) {
                    Ok(m) => m,
                    Err(e) if at.is_some() => {
                        return format!("{n} not reloaded, the loaded one stays: {e}");
                    }
                    Err(e) => return format!("{n} not loaded: {e}"),
                };
                // Then the old one goes: a plugin never runs twice at once.
                let old = at.map(|i| list.remove(i));
                let from = old
                    .as_ref()
                    .map(|p| format!("{} -> ", p.version))
                    .unwrap_or_default();
                if let Some(old) = old {
                    old.unload();
                }
                match self.start(n, mapped) {
                    Ok(p) => {
                        let new: Vec<_> = p
                            .commands
                            .iter()
                            .map(|c| root(&c.usage).to_owned())
                            .filter(|r| !self.is_registered(r))
                            .collect();
                        let mut answer = format!(
                            "{} {n} {from}{} (load #{}) in {} ms",
                            if from.is_empty() {
                                "Loaded"
                            } else {
                                "Reloaded"
                            },
                            p.version,
                            p.load,
                            started.elapsed().as_millis()
                        );
                        if !new.is_empty() {
                            answer += &format!(
                                "; new commands {} reach players once the host refreshes the command tree",
                                new.join(", ")
                            );
                        }
                        list.push(p);
                        answer
                    }
                    Err(e) => format!("{n} not loaded: {e}"),
                }
            }
            _ => "Usage: /plugin list | load <name> | unload <name> | reload <name>".into(),
        }
    }

    /// Calls `f` for each loaded plugin that has the field ending at `end`, with the `Call` for
    /// it, until `f` answers true. True when one did.
    fn each(
        &self,
        calls: &mut dyn Calls,
        end: usize,
        mut f: impl FnMut(&abi::PluginV1, *mut abi::Call) -> bool,
    ) -> bool {
        let Ok(list) = self.list.read() else {
            return false;
        };
        for p in list.iter() {
            let raw = p.plugin();
            if !raw.has(end) {
                continue;
            }
            let mut cx = CallCx {
                calls: &mut *calls,
                plugins: self,
                plugin: &p.name,
                config_dir: &p.config_dir,
                tasks: &p.tasks,
                chat: None,
            };
            if f(raw, (&mut cx as *mut CallCx<'_>).cast()) {
                return true;
            }
        }
        false
    }

    fn allowed(calls: &mut dyn Calls, sender: Option<Sender<'_>>, c: &PluginCommand) -> bool {
        let Some(s) = sender else {
            return true;
        };
        if s.permission >= c.level {
            return true;
        }
        !c.node.is_empty() && calls.has_permission(s.uuid, &c.node, c.level)
    }

    /// A command for the plugins: true when one took it. The sender needs the level the plugin
    /// gave the command, or its permission node.
    pub fn command(&self, calls: &mut dyn Calls, sender: Option<Sender<'_>>, line: &str) -> bool {
        let Ok(list) = self.list.read() else {
            return false;
        };
        let player = sender.map(raw);
        for p in list.iter() {
            let ok = p
                .commands
                .iter()
                .filter(|c| root(&c.usage) == root(line))
                .any(|c| Self::allowed(&mut *calls, sender, c));
            if !ok {
                continue;
            }
            let mut cx = CallCx {
                calls: &mut *calls,
                plugins: self,
                plugin: &p.name,
                config_dir: &p.config_dir,
                tasks: &p.tasks,
                chat: None,
            };
            let raw = p.plugin();
            let took = (raw.command)(
                raw.state,
                &HOST,
                (&mut cx as *mut CallCx<'_>).cast(),
                player_ptr(&player),
                abi::Str::new(line),
            );
            if took {
                return true;
            }
        }
        false
    }

    /// A player joined: every plugin hears it.
    pub fn player_joined(&self, calls: &mut dyn Calls, player: Sender<'_>) {
        let player = raw(player);
        self.each(
            calls,
            field_end!(abi::PluginV1, player_joined),
            |p, call| {
                (p.player_joined)(p.state, &HOST, call, &player);
                false
            },
        );
    }

    /// A player is leaving: every plugin hears it.
    pub fn player_quit(&self, calls: &mut dyn Calls, player: Sender<'_>) {
        let player = raw(player);
        self.each(calls, field_end!(abi::PluginV1, player_quit), |p, call| {
            (p.player_quit)(p.state, &HOST, call, &player);
            false
        });
    }

    /// A chat line: each plugin sees the line as the ones before left it.
    pub fn chat(&self, calls: &mut dyn Calls, player: Sender<'_>, message: &str) -> Chat {
        let player = raw(player);
        let mut line = message.to_owned();
        let end = field_end!(abi::PluginV1, chat);
        // `line` is what `rewrite_chat` writes; each plugin reads `current`, a copy of it taken
        // before its call.
        let mut current = line.clone();
        let cancelled = {
            let Ok(list) = self.list.read() else {
                return Chat::Unchanged;
            };
            let mut cancelled = false;
            for p in list.iter() {
                let raw = p.plugin();
                if !raw.has(end) {
                    continue;
                }
                let mut cx = CallCx {
                    calls: &mut *calls,
                    plugins: self,
                    plugin: &p.name,
                    config_dir: &p.config_dir,
                    tasks: &p.tasks,
                    chat: Some(&mut line),
                };
                let verdict = (raw.chat)(
                    raw.state,
                    &HOST,
                    (&mut cx as *mut CallCx<'_>).cast(),
                    &player,
                    abi::Str::new(&current),
                );
                current.clone_from(&line);
                if verdict == abi::CANCEL {
                    cancelled = true;
                    break;
                }
            }
            cancelled
        };
        if cancelled {
            Chat::Cancelled
        } else if line == message {
            Chat::Unchanged
        } else {
            Chat::Rewritten(line)
        }
    }

    /// The start of a server tick: every plugin's `tick`, then the tasks that are due.
    pub fn tick(&self, calls: &mut dyn Calls) {
        let now = self.ticks.fetch_add(1, Ordering::Relaxed) + 1;
        self.each(calls, field_end!(abi::PluginV1, tick), |p, call| {
            (p.tick)(p.state, &HOST, call, now);
            false
        });
        let Ok(list) = self.list.read() else {
            return;
        };
        for p in list.iter() {
            let raw = p.plugin();
            if !raw.has(field_end!(abi::PluginV1, task)) {
                continue;
            }
            // Taken out first: a task may schedule or cancel tasks.
            let due: Vec<u64> = {
                let Ok(mut tasks) = p.tasks.lock() else {
                    continue;
                };
                let due = tasks
                    .iter()
                    .filter(|t| t.due <= now)
                    .map(|t| t.id)
                    .collect();
                tasks.retain_mut(|t| {
                    if t.due > now {
                        return true;
                    }
                    t.due = now + t.period;
                    t.period > 0
                });
                due
            };
            for id in due {
                let mut cx = CallCx {
                    calls: &mut *calls,
                    plugins: self,
                    plugin: &p.name,
                    config_dir: &p.config_dir,
                    tasks: &p.tasks,
                    chat: None,
                };
                (raw.task)(raw.state, &HOST, (&mut cx as *mut CallCx<'_>).cast(), id);
            }
        }
    }

    /// A block about to be broken; true when a plugin cancelled it.
    pub fn block_break(
        &self,
        calls: &mut dyn Calls,
        player: Option<Sender<'_>>,
        block: BlockAt<'_>,
    ) -> bool {
        let player = player.map(raw);
        let block = raw_block(block);
        self.each(calls, field_end!(abi::PluginV1, block_break), |p, call| {
            (p.block_break)(p.state, &HOST, call, player_ptr(&player), &block) == abi::CANCEL
        })
    }

    /// A block about to be placed; true when a plugin cancelled it.
    pub fn block_place(
        &self,
        calls: &mut dyn Calls,
        player: Sender<'_>,
        block: BlockAt<'_>,
    ) -> bool {
        let player = raw(player);
        let block = raw_block(block);
        self.each(calls, field_end!(abi::PluginV1, block_place), |p, call| {
            (p.block_place)(p.state, &HOST, call, &player, &block) == abi::CANCEL
        })
    }

    /// A player about to be hurt; plugins may change `amount`. True when one cancelled it.
    pub fn player_damage(&self, calls: &mut dyn Calls, hit: Hit<'_>, amount: &mut f32) -> bool {
        let damage = abi::Damage {
            victim: hit.victim.into(),
            victim_entity: hit.victim_entity,
            attacker: hit.attacker.unwrap_or(0).into(),
            attacker_entity: hit.attacker_entity,
            source: abi::Str::new(hit.source),
            damage_type: abi::Str::new(hit.damage_type),
        };
        self.each(
            calls,
            field_end!(abi::PluginV1, player_damage),
            |p, call| (p.player_damage)(p.state, &HOST, call, &damage, &mut *amount) == abi::CANCEL,
        )
    }

    /// A console or RCON line before any command sees it; true when a plugin cancelled it.
    pub fn console_line(&self, calls: &mut dyn Calls, source: &str, line: &str) -> bool {
        self.each(calls, field_end!(abi::PluginV1, console_line), |p, call| {
            (p.console_line)(
                p.state,
                &HOST,
                call,
                abi::Str::new(source),
                abi::Str::new(line),
            ) == abi::CANCEL
        })
    }

    /// Unloads every plugin (the server stops).
    pub fn unload_all(&self) {
        if let Ok(mut list) = self.list.write() {
            for p in list.drain(..) {
                p.unload();
            }
        }
    }
}
