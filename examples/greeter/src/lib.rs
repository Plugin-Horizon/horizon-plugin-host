//! An example server plugin: `/greet`, a welcome and goodbye, a chat filter, protected bedrock
//! layers, no TNT for players below level 2, a damage cap between players, a periodic announcement
//! and a message of the day kept in `plugins/greeter/motd.txt`. It uses every event of ABI 1.
//!
//! Build with `cargo build --release -p greeter`, copy `target/release/libgreeter.so` to the
//! server's `plugins/greeter.so` (`.dylib` on macOS), and `/plugin reload greeter`.
use horizon_plugin_abi::{
    BlockInfo, CommandSpec, Cx, DamageInfo, Flow, PlayerInfo, ServerPlugin, export_plugin,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The task id of the announcement.
const ANNOUNCE: u64 = 1;
/// Five minutes at 20 ticks a second.
const ANNOUNCE_EVERY: u64 = 20 * 60 * 5;

#[derive(Default)]
struct Greeter {
    /// How often `/greet` ran since this copy was loaded: a reload starts again at 0.
    calls: AtomicU32,
    /// The last server tick the `tick` event saw.
    ticks: AtomicU64,
    /// Read from `motd.txt` on the first call that can reach the config folder.
    motd: Mutex<Option<String>>,
    announcing: Mutex<bool>,
}

impl Greeter {
    fn motd(&self, cx: &mut Cx<'_>) -> String {
        let mut motd = self.motd.lock().unwrap();
        motd.get_or_insert_with(|| {
            let file = cx.config_dir().map(|d| d.join("motd.txt"));
            match file.as_ref().map(std::fs::read_to_string) {
                Some(Ok(text)) => text.trim().to_owned(),
                _ => {
                    let text = "Welcome to the server!".to_owned();
                    if let Some(file) = file {
                        let _ = std::fs::write(file, &text);
                    }
                    text
                }
            }
        })
        .clone()
    }

    fn start_announcing(&self, cx: &mut Cx<'_>) {
        let mut on = self.announcing.lock().unwrap();
        if !*on {
            *on = cx
                .schedule(ANNOUNCE, ANNOUNCE_EVERY, ANNOUNCE_EVERY)
                .is_some();
        }
    }
}

impl ServerPlugin for Greeter {
    const NAME: &'static str = "greeter";
    const VERSION: &'static str = "1.0.0";

    fn commands(&self) -> Vec<CommandSpec> {
        vec![
            CommandSpec::new("greet [name:word]", 0),
            CommandSpec::new("greetmotd <text:text>", 3).node("greeter.motd"),
        ]
    }

    fn command(&self, cx: &mut Cx<'_>, sender: Option<&PlayerInfo<'_>>, line: &str) -> bool {
        let (root, rest) = line.split_once(' ').unwrap_or((line, ""));
        match root {
            "greet" => {
                let n = self.calls.fetch_add(1, Ordering::Relaxed) + 1;
                let who = if rest.is_empty() {
                    sender.map_or("console", |s| s.name)
                } else {
                    rest
                };
                let tick = self.ticks.load(Ordering::Relaxed);
                cx.reply(&format!("Hello, {who} (call {n} of this load, tick {tick})"));
                true
            }
            "greetmotd" => {
                if let Some(dir) = cx.config_dir() {
                    let _ = std::fs::write(dir.join("motd.txt"), rest);
                }
                *self.motd.lock().unwrap() = Some(rest.to_owned());
                cx.reply("Message of the day set");
                true
            }
            _ => false,
        }
    }

    fn player_joined(&self, cx: &mut Cx<'_>, player: &PlayerInfo<'_>) {
        self.start_announcing(cx);
        let motd = self.motd(cx);
        cx.tell(player.uuid, &motd);
        cx.log(&format!("greeted {}", player.name));
    }

    fn player_quit(&self, cx: &mut Cx<'_>, player: &PlayerInfo<'_>) {
        cx.broadcast(&format!("{} left. See you soon!", player.name));
    }

    fn chat(&self, cx: &mut Cx<'_>, _player: &PlayerInfo<'_>, message: &str) -> Flow {
        if message.chars().filter(|c| c.is_uppercase()).count() > 20 {
            cx.rewrite_chat(&message.to_lowercase());
        }
        Flow::Continue
    }

    fn tick(&self, _cx: &mut Cx<'_>, tick: u64) {
        self.ticks.store(tick, Ordering::Relaxed);
    }

    fn task(&self, cx: &mut Cx<'_>, id: u64) {
        if id == ANNOUNCE {
            cx.broadcast("Tip: /greet says hello.");
        }
    }

    fn block_break(
        &self,
        _cx: &mut Cx<'_>,
        player: Option<&PlayerInfo<'_>>,
        block: &BlockInfo<'_>,
    ) -> Flow {
        // Players below level 2 keep their hands off the bottom layers.
        match player {
            Some(p) if p.permission < 2 && block.pos[1] <= -60 => Flow::Cancel,
            _ => Flow::Continue,
        }
    }

    fn block_place(
        &self,
        _cx: &mut Cx<'_>,
        player: &PlayerInfo<'_>,
        block: &BlockInfo<'_>,
    ) -> Flow {
        // TNT is for operators.
        match block.item {
            "minecraft:tnt" if player.permission < 2 => Flow::Cancel,
            _ => Flow::Continue,
        }
    }

    fn player_damage(&self, _cx: &mut Cx<'_>, damage: &DamageInfo<'_>, amount: &mut f32) -> Flow {
        // A hit between players hurts at most ten points.
        if damage.damage_type == "minecraft:player_attack" {
            *amount = (*amount).min(10.0);
        }
        Flow::Continue
    }

    fn console_line(&self, _cx: &mut Cx<'_>, source: &str, line: &str) -> Flow {
        // Over RCON the message of the day stays as it is; the console may still set it.
        if source == "rcon" && line.starts_with("greetmotd") {
            Flow::Cancel
        } else {
            Flow::Continue
        }
    }
}

impl Drop for Greeter {
    fn drop(&mut self) {
        eprintln!(
            "[greeter {}] unloaded after {} calls",
            Self::VERSION,
            self.calls.load(Ordering::Relaxed)
        );
    }
}

export_plugin!(Greeter, Greeter::default());
