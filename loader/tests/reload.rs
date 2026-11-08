//! Builds a plugin, loads it, rebuilds it with another answer, reloads it: the new code answers,
//! the old state and tasks were dropped, and a file that is not a plugin is refused. Then every
//! event and host function of ABI 1 once. No server needed.
use horizon_plugin_loader::{BlockAt, Calls, Chat, Hit, Plugins, Sender};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Default)]
struct Record {
    lines: Vec<String>,
    /// Nodes `has_permission` grants.
    granted: Vec<&'static str>,
}

impl Calls for Record {
    fn log(&mut self, plugin: &str, line: &str) {
        self.lines.push(format!("log {plugin}: {line}"));
    }
    fn reply(&mut self, _plugin: &str, line: &str) -> bool {
        self.lines.push(format!("reply: {line}"));
        true
    }
    fn tell(&mut self, player: u128, line: &str) -> bool {
        self.lines.push(format!("tell {player}: {line}"));
        true
    }
    fn broadcast(&mut self, line: &str) {
        self.lines.push(format!("broadcast: {line}"));
    }
    fn run_command(&mut self, line: &str) -> bool {
        self.lines.push(format!("run: {line}"));
        false
    }
    fn has_permission(&mut self, _player: u128, node: &str, _default_level: u8) -> bool {
        self.granted.contains(&node)
    }
}

const PROBE: Sender<'static> = Sender {
    uuid: 7,
    name: "Probe",
    permission: 0,
};

/// A fixture plugin answering `/greet` with `answer`, whose `Drop` writes `dropped` next to
/// the plugins folder.
fn build(work: &Path, answer: &str, version: &str) -> PathBuf {
    build_with(
        work,
        answer,
        version,
        "export_plugin!(Greet, Greet::default());",
    )
}

/// A build for another ABI major: its entry point has another name.
fn build_v2(work: &Path) -> PathBuf {
    build_with(
        work,
        "v2",
        "9.0.0",
        "#[unsafe(no_mangle)] pub extern \"C\" fn horizon_server_plugin_v2() -> *mut PluginV1 { export::<Greet>(Greet::default) }",
    )
}

fn build_with(work: &Path, answer: &str, version: &str, export: &str) -> PathBuf {
    let abi = Path::new(env!("CARGO_MANIFEST_DIR")).join("../abi");
    let src = work.join("fixture");
    std::fs::create_dir_all(src.join("src")).unwrap();
    std::fs::write(
        src.join("Cargo.toml"),
        format!(
            "[package]\nname = \"greet\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[lib]\ncrate-type = [\"cdylib\"]\n\
             [dependencies]\nhorizon-plugin-abi = {{ path = {:?} }}\n[workspace]\n",
            abi.display().to_string()
        ),
    )
    .unwrap();
    let marker = work.join("dropped");
    std::fs::write(
        src.join("src/lib.rs"),
        format!(
            r#"use horizon_plugin_abi::*;
#[derive(Default)]
struct Greet;
impl ServerPlugin for Greet {{
    const NAME: &'static str = "greet";
    const VERSION: &'static str = "{version}";
    fn commands(&self) -> Vec<CommandSpec> {{
        vec![CommandSpec::new("greet", 0), CommandSpec::new("secret", 4), CommandSpec::new("staff", 4).node("greet.staff")]
    }}
    fn command(&self, cx: &mut Cx<'_>, s: Option<&PlayerInfo<'_>>, line: &str) -> bool {{
        match line {{
            "boom" => panic!("a plugin panic stays in the plugin"),
            "greet every" => {{ cx.schedule(1, 2, 3); }}
            "greet once" => {{ let h = cx.schedule(2, 1, 0); cx.reply(&format!("handle {{}}", h.is_some())); }}
            "greet dir" => {{
                let dir = cx.config_dir().unwrap();
                std::fs::write(dir.join("seen.txt"), "{version}").unwrap();
                cx.reply(&dir.file_name().unwrap().to_string_lossy());
            }}
            "greet perm" => {{
                let p = cx.has_permission(s.map_or(0, |s| s.uuid), "greet.staff", 4);
                cx.reply(&format!("perm {{p:?}}"));
            }}
            _ => {{ cx.reply("{answer}"); }}
        }}
        true
    }}
    fn player_joined(&self, cx: &mut Cx<'_>, p: &PlayerInfo<'_>) {{ cx.tell(p.uuid, &format!("hi {{}}", p.name)); }}
    fn player_quit(&self, cx: &mut Cx<'_>, p: &PlayerInfo<'_>) {{ cx.broadcast(&format!("bye {{}}", p.name)); }}
    fn chat(&self, cx: &mut Cx<'_>, _p: &PlayerInfo<'_>, message: &str) -> Flow {{
        if message.contains("spam") {{ return Flow::Cancel; }}
        if message.contains("darn") {{ cx.rewrite_chat(&message.replace("darn", "****")); }}
        Flow::Continue
    }}
    fn tick(&self, _cx: &mut Cx<'_>, _tick: u64) {{}}
    fn task(&self, cx: &mut Cx<'_>, id: u64) {{ cx.broadcast(&format!("task {{id}}")); }}
    fn block_break(&self, _cx: &mut Cx<'_>, p: Option<&PlayerInfo<'_>>, b: &BlockInfo<'_>) -> Flow {{
        if b.pos[1] < 0 && p.is_some() {{ Flow::Cancel }} else {{ Flow::Continue }}
    }}
    fn block_place(&self, _cx: &mut Cx<'_>, _p: &PlayerInfo<'_>, b: &BlockInfo<'_>) -> Flow {{
        if b.item == "minecraft:tnt" {{ Flow::Cancel }} else {{ Flow::Continue }}
    }}
    fn player_damage(&self, _cx: &mut Cx<'_>, d: &DamageInfo<'_>, amount: &mut f32) -> Flow {{
        if d.source == "melee" {{ *amount /= 2.0; Flow::Continue }} else {{ Flow::Cancel }}
    }}
    fn console_line(&self, cx: &mut Cx<'_>, source: &str, line: &str) -> Flow {{
        if line.starts_with("stop") && source == "rcon" {{ cx.log("no stop over rcon"); Flow::Cancel }} else {{ Flow::Continue }}
    }}
}}
impl Drop for Greet {{ fn drop(&mut self) {{ std::fs::write({marker:?}, "{version}").unwrap(); }} }}
{export}
"#
        ),
    )
    .unwrap();
    let status = Command::new(env!("CARGO"))
        .args(["build", "--quiet", "--offline", "--manifest-path"])
        .arg(src.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", work.join("target"))
        .status()
        .unwrap();
    assert!(status.success());
    work.join("target/debug").join(format!(
        "{}greet{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

fn install(built: &Path, plugins: &Path, name: &str) {
    std::fs::copy(
        built,
        plugins.join(format!("{name}{}", std::env::consts::DLL_SUFFIX)),
    )
    .unwrap();
}

fn ask_with(plugins: &Plugins, record: &mut Record, line: &str, permission: u8) -> bool {
    let sender = Sender {
        permission,
        ..PROBE
    };
    plugins.command(record, Some(sender), line)
}

fn ask(plugins: &Plugins, line: &str, permission: u8) -> Vec<String> {
    let mut record = Record::default();
    ask_with(plugins, &mut record, line, permission);
    record.lines
}

fn ticks(plugins: &Plugins, n: usize) -> Vec<String> {
    let mut record = Record::default();
    for _ in 0..n {
        plugins.tick(&mut record);
    }
    record.lines
}

fn fresh(name: &str) -> (PathBuf, PathBuf) {
    let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&work);
    let dir = work.join("plugins");
    std::fs::create_dir_all(&dir).unwrap();
    (work, dir)
}

#[test]
fn a_rebuilt_plugin_answers_after_a_reload_without_a_restart() {
    let (work, dir) = fresh("reload");
    install(&build(&work, "one", "1.0.0"), &dir, "greet");
    let plugins = Plugins::new(dir.clone());
    assert_eq!(plugins.load_all(), ["loaded greet 1.0.0"]);
    assert_eq!(ask(&plugins, "greet", 0), ["reply: one"]);
    // The level the plugin gave a command is checked before the plugin is asked.
    assert!(ask(&plugins, "secret", 0).is_empty());
    assert_eq!(ask(&plugins, "secret", 4), ["reply: one"]);
    // A panic is caught in the plugin: the command is not taken, the host goes on.
    assert!(ask(&plugins, "boom", 0).is_empty());
    let mut record = Record::default();
    plugins.player_joined(&mut record, PROBE);
    assert_eq!(record.lines, ["tell 7: hi Probe"]);

    // A repeating task of the first build.
    assert!(ask(&plugins, "greet every", 0).is_empty());
    assert_eq!(ticks(&plugins, 2), ["broadcast: task 1"]);

    // Rebuilt in place while loaded, then reloaded: the new code answers, the old state was dropped.
    install(&build(&work, "two", "2.0.0"), &dir, "greet");
    let answer = plugins.admin("plugin reload greet");
    assert!(
        answer.starts_with("Reloaded greet 1.0.0 -> 2.0.0 (load #2)"),
        "{answer}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("dropped")).unwrap(),
        "1.0.0"
    );
    assert_eq!(ask(&plugins, "greet", 0), ["reply: two"]);
    // The old build's task went with it.
    assert!(ticks(&plugins, 6).is_empty());
    assert!(
        plugins
            .admin("plugin list")
            .contains("greet 2.0.0 (load #2)")
    );

    // Unloaded: nothing answers; loaded again: the same file answers.
    assert_eq!(plugins.admin("plugin unload greet"), "Unloaded greet");
    assert!(ask(&plugins, "greet", 0).is_empty());
    assert!(
        plugins
            .admin("plugin load greet")
            .starts_with("Loaded greet 2.0.0 (load #3)")
    );
    assert_eq!(ask(&plugins, "greet", 0), ["reply: two"]);

    // A library that is not a plugin, and a plugin under another name, are refused.
    install(&build(&work, "two", "2.0.0"), &dir, "other");
    let answer = plugins.admin("plugin load other");
    assert_eq!(
        answer,
        "other not loaded: the file other holds plugin greet"
    );
    std::fs::write(
        dir.join(format!("junk{}", std::env::consts::DLL_SUFFIX)),
        b"not a library",
    )
    .unwrap();
    assert!(
        plugins
            .admin("plugin load junk")
            .starts_with("junk not loaded: dlopen:")
    );
    plugins.unload_all();
    assert!(ask(&plugins, "greet", 0).is_empty());
}

#[test]
fn every_event_and_host_function_reaches_the_plugin() {
    let (work, dir) = fresh("events");
    install(&build(&work, "one", "1.0.0"), &dir, "greet");
    let plugins = Plugins::new(dir.clone());
    plugins.load_all();
    let mut r = Record::default();

    // Chat: cancel, rewrite, unchanged.
    assert_eq!(plugins.chat(&mut r, PROBE, "buy spam now"), Chat::Cancelled);
    assert_eq!(
        plugins.chat(&mut r, PROBE, "oh darn"),
        Chat::Rewritten("oh ****".into())
    );
    assert_eq!(plugins.chat(&mut r, PROBE, "hello"), Chat::Unchanged);

    // Quit.
    plugins.player_quit(&mut r, PROBE);
    assert_eq!(r.lines, ["broadcast: bye Probe"]);

    // Blocks: break below y 0 by a player is cancelled, by nobody is not; TNT is not placed.
    let block = |y, item| BlockAt {
        dimension: "minecraft:overworld",
        pos: [1, y, 2],
        state: 1,
        item,
    };
    assert!(plugins.block_break(&mut r, Some(PROBE), block(-5, "")));
    assert!(!plugins.block_break(&mut r, None, block(-5, "")));
    assert!(!plugins.block_break(&mut r, Some(PROBE), block(64, "")));
    assert!(plugins.block_place(&mut r, PROBE, block(64, "minecraft:tnt")));
    assert!(!plugins.block_place(&mut r, PROBE, block(64, "minecraft:stone")));

    // Damage: melee halved, an arrow cancelled.
    let hit = |source| Hit {
        victim: 7,
        victim_entity: 1,
        attacker: Some(8),
        attacker_entity: 2,
        source,
        damage_type: "minecraft:player_attack",
    };
    let mut amount = 4.0;
    assert!(!plugins.player_damage(&mut r, hit("melee"), &mut amount));
    assert_eq!(amount, 2.0);
    assert!(plugins.player_damage(&mut r, hit("minecraft:arrow"), &mut amount));

    // Console lines.
    let mut r = Record::default();
    assert!(plugins.console_line(&mut r, "rcon", "stop"));
    assert_eq!(r.lines, ["log greet: no stop over rcon"]);
    assert!(!plugins.console_line(&mut r, "console", "stop"));

    // Permission nodes: `staff` needs level 4 or the node `greet.staff`.
    assert!(ask(&plugins, "staff", 0).is_empty());
    let mut r = Record {
        granted: vec!["greet.staff"],
        ..Record::default()
    };
    assert!(ask_with(&plugins, &mut r, "staff", 0));
    assert_eq!(r.lines, ["reply: one"]);
    let mut r = Record {
        granted: vec!["greet.staff"],
        ..Record::default()
    };
    ask_with(&plugins, &mut r, "greet perm", 0);
    assert_eq!(r.lines, ["reply: perm Some(true)"]);

    // A one-shot task runs once, on the tick after its delay.
    assert_eq!(ask(&plugins, "greet once", 0), ["reply: handle true"]);
    assert_eq!(ticks(&plugins, 3), ["broadcast: task 2"]);

    // The config folder is plugins/greet/ and survives a reload.
    assert_eq!(ask(&plugins, "greet dir", 0), ["reply: greet"]);
    assert_eq!(
        std::fs::read_to_string(dir.join("greet/seen.txt")).unwrap(),
        "1.0.0"
    );
    plugins.admin("plugin reload greet");
    assert!(dir.join("greet/seen.txt").is_file());
    plugins.unload_all();
}

#[test]
fn a_plugin_of_another_abi_is_refused_and_the_loaded_one_stays() {
    let (work, dir) = fresh("abi");
    install(&build(&work, "one", "1.0.0"), &dir, "greet");
    let plugins = Plugins::new(dir.clone());
    plugins.load_all();
    let v2 = build_v2(&work);
    install(&v2, &dir, "greet");
    assert_eq!(
        plugins.admin("plugin reload greet"),
        "greet not reloaded, the loaded one stays: no horizon_server_plugin_v1: not a plugin of ABI 1"
    );
    assert_eq!(ask(&plugins, "greet", 0), ["reply: one"]);
    plugins.unload_all();
}
