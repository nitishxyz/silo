//! `silo do` scripts and actions, `silo config`, `silo actions`,
//! `silo keymap` and `silo share` through the binary, against a server on
//! a temp home whose `config/init.lua` the test writes.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

use serde_json::{Value, json};
use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    config::ConfigState,
    control::{Created, Method, SpawnSpec},
    model::BlockKind,
    paths,
};

const INIT: &str = r#"silo.bind("cmd-e", "SplitRight")
silo.bind("ctrl-b c", "window.create")
silo.mode("resize", { exclusive = true })
silo.bind("resize/left", "MovePaneEdgeLeft")
silo.action{
  name = "greet", title = "Greet Someone", category = "Test",
  args = { name = "string", times = "integer?" },
  run = function(ctx, a)
    return { text = string.rep("hi " .. a.name .. " ", a.times or 1), origin = ctx.origin }
  end,
}
"#;

/// `silo` with `args` on `home`, `stdin` piped in.
fn silo_in(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_silo"))
        .args(args)
        .env_remove("OTTO_SILO_HOME")
        .env_remove("SILO_BLOCK")
        .env_remove("SILO_SESSION")
        .env_remove("SILO_CONFIG")
        .env_remove("SILO_TOKEN")
        .env("SILO_HOME", home)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(text) = stdin {
        child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

async fn run_in(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let (home, args, stdin): (PathBuf, Vec<String>, Option<String>) =
        (home.to_owned(), args.iter().map(|&a| a.to_owned()).collect(), stdin.map(str::to_owned));
    tokio::task::spawn_blocking(move || silo_in(&home, &args.iter().map(String::as_str).collect::<Vec<_>>(), stdin.as_deref())).await.unwrap()
}

async fn run(home: &Path, args: &[&str]) -> Output {
    run_in(home, args, None).await
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Exit 0; stdout as JSON.
fn json_of(out: &Output) -> Value {
    assert!(out.status.success(), "exit {:?}: {}", out.status.code(), text(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", text(&out.stdout)))
}

#[tokio::test(flavor = "multi_thread")]
async fn scripts_actions_config_and_share() {
    let home = PathBuf::from(format!("/tmp/silo-cli-lua-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(home.join("config/init.lua"), INIT).unwrap();
    let config = silo_server::Config { home: home.clone(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    tokio::time::timeout(Duration::from_secs(10), async {
        while !silo_client::server_running(&home).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server did not start");
    let (control, _events) = Control::connect(&paths::socket(&home), "test").await.unwrap();
    // The config loads after the server starts.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(state) = control.call::<ConfigState>(Method::ConfigGet {}).await
                && !state.actions.is_empty()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the config did not load");
    let spawn = SpawnSpec { command: vec!["sleep".into(), "100".into()], ..SpawnSpec::default() };
    let created: Created =
        control.call(Method::SessionCreate { name: Some("work".into()), spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();

    // A script file returning a value.
    let script = home.join("value.lua");
    std::fs::write(&script, "local t = { a = 1, b = 'x' }\nreturn t\n").unwrap();
    assert_eq!(json_of(&run(&home, &["do", script.to_str().unwrap()]).await), json!({"a": 1, "b": "x"}));
    // Nothing returned, nothing printed.
    let out = run(&home, &["do", "-e", "local x = 1"]).await;
    assert!(out.status.success() && out.stdout.is_empty(), "{}", text(&out.stderr));

    // `key=value` typing; `--args` wins.
    let out = run(&home, &["do", "-e", "return silo.args", "n=2", "on=true", "off=false", "x=null", "s=hello", "f=1.5", "z=007"]).await;
    assert_eq!(json_of(&out), json!({"n": 2, "on": true, "off": false, "s": "hello", "f": 1.5, "z": "007"}));
    let out = run(&home, &["do", "-e", "return silo.args", "n=2", "--args", r#"{"n": [5]}"#]).await;
    assert_eq!(json_of(&out), json!({"n": [5]}));

    // Exit codes: silo.stop, an error.
    let out = run(&home, &["do", "-e", "silo.stop(3)"]).await;
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    let out = run(&home, &["do", "-e", "error('it broke', 0)"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("Error: it broke"), "{}", text(&out.stderr));
    let out = run(&home, &["do", "-e", "local a = 1\nerror('second line')"]).await;
    assert!(out.status.code() == Some(1) && text(&out.stderr).contains("-e:2: second line"), "{}", text(&out.stderr));
    let out = run(&home, &["do", "-e", "return ("]).await;
    assert!(out.status.code() == Some(1) && text(&out.stderr).starts_with("Error: "), "a syntax error: {}", text(&out.stderr));

    // stdin.
    let out = run_in(&home, &["do", "-"], Some("return 1 + 1\n")).await;
    assert_eq!(json_of(&out), json!(2));

    // Calls into Silo.
    let out = run(&home, &["do", "-e", "local list, err = silo.session.list{}\nassert(list, err)\nreturn list"]).await;
    let list = json_of(&out);
    assert!(list.as_array().is_some_and(|l| l.iter().any(|s| s["id"] == created.session_id.as_str())), "{list}");
    let out = run(&home, &["do", "-s", "work", "-e", "return silo.session_id"]).await;
    assert_eq!(json_of(&out), json!(created.session_id));
    let out = run(&home, &["do", "-b", &created.block_id, "-e", "return { silo.session_id, silo.block_id }"]).await;
    assert_eq!(json_of(&out), json!([created.session_id, created.block_id]), "the session is looked up from the block");

    // A handler keeps a script running until --for.
    let out = run(&home, &["do", "--for", "1s", "-e", "silo.on('block.bell', function() end)"]).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("listening: 1 handler; Ctrl-C to stop"), "{}", text(&out.stderr));
    // --fail-fast: a failing timer ends it.
    let out = run(&home, &["do", "--fail-fast", "--for", "10s", "-e", "silo.after(0.1, function() error('tick', 0) end)"]).await;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));

    // An action of the server's config.
    let out = run(&home, &["do", "greet", "name=Ana", "times=2"]).await;
    assert_eq!(json_of(&out), json!({"text": "hi Ana hi Ana ", "origin": "cli"}));
    let out = run(&home, &["do", "greet", "name=42"]).await;
    assert_eq!(json_of(&out)["text"], "hi 42 ", "a declared string stays one");
    // A bare value is the one required argument; flags name the others.
    let out = run(&home, &["do", "greet", "Ana", "--times", "2"]).await;
    assert_eq!(json_of(&out)["text"], "hi Ana hi Ana ");
    let out = run(&home, &["do", "greet", "--name=Ana"]).await;
    assert_eq!(json_of(&out)["text"], "hi Ana ");
    let out = run(&home, &["do", "greet", "Ana", "--loud"]).await;
    assert!(out.status.code() == Some(1) && text(&out.stderr).contains("unknown flag --loud"), "{}", text(&out.stderr));
    let out = run(&home, &["do", "greet", "name=Ana", "times=many"]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).starts_with("Error: ") && text(&out.stderr).contains("times"), "{}", text(&out.stderr));
    let out = run(&home, &["do", "greet"]).await;
    assert!(out.status.code() == Some(1) && text(&out.stderr).contains("name"), "{}", text(&out.stderr));
    let out = run(&home, &["do", "nope"]).await;
    assert!(out.status.code() == Some(1) && text(&out.stderr).contains("no action nope"), "{}", text(&out.stderr));
    let out = run(&home, &["do", "greet", "--for", "1s"]).await;
    assert_eq!(out.status.code(), Some(1), "--for is for scripts");
    // An action of a file, loaded here.
    let file = home.join("tools.lua");
    std::fs::write(&file, "silo.action{ name = 'twice', title = 'Twice', args = { n = 'number' }, run = function(ctx, a) return a.n * 2 end }\n")
        .unwrap();
    let out = run(&home, &["do", file.to_str().unwrap(), "--action", "twice", "n=21"]).await;
    assert_eq!(json_of(&out), json!(42));

    // A method, as before.
    let out = run(&home, &["do", "window.step", &created.session_id, "1"]).await;
    assert_eq!(json_of(&out), json!({}));

    // silo actions, silo keymap.
    let out = run(&home, &["actions"]).await;
    let listed = text(&out.stdout);
    assert!(out.status.success() && listed.contains("greet") && listed.contains("Greet Someone") && listed.contains("name:string"), "{listed}");
    let actions = json_of(&run(&home, &["actions", "--json"]).await);
    assert_eq!(actions[0]["name"], "greet", "{actions}");
    let out = run(&home, &["keymap"]).await;
    let keymap = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    for want in ["cmd-e", "command SplitRight", "ctrl-b c", "method window.create", "resize/left", "exclusive"] {
        assert!(keymap.contains(want), "{want}: {keymap}");
    }
    let keymap = json_of(&run(&home, &["keymap", "--json"]).await);
    assert_eq!(keymap["modes"][0]["name"], "resize");

    // silo config show, reload.
    let out = run(&home, &["config", "show"]).await;
    assert!(out.status.success() && text(&out.stdout).contains("1 action"), "{}", text(&out.stdout));
    let out = run(&home, &["config", "reload"]).await;
    assert!(out.status.success() && text(&out.stdout).contains("3 bindings"), "{}", text(&out.stdout));

    // silo share: a link, listed, then revoked.
    let out = run(&home, &["share", "work", "--label", "for Ana"]).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    let link = text(&out.stdout);
    assert!(link.trim().contains("#silos1"), "{link}");
    let out = run(&home, &["share", "--list"]).await;
    let listed = text(&out.stdout);
    let line = listed.lines().find(|l| l.contains("for Ana")).unwrap_or_else(|| panic!("{listed}"));
    assert!(line.contains("link") && line.contains(&format!("session {}", created.session_id)), "{line}");
    let id = line.split_whitespace().next().unwrap().to_owned();
    assert!(text(&run(&home, &["grants"]).await.stdout).contains(&id), "silo grants lists it");
    let out = run(&home, &["share", "--revoke", &id]).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!text(&run(&home, &["share", "--list"]).await.stdout).contains(&id));
    let out = run(&home, &["share", "--block", &created.block_id, "--rights", "interact"]).await;
    assert!(out.status.success() && text(&out.stdout).contains("silos1"), "{}", text(&out.stderr));
    let out = run(&home, &["share"]).await;
    assert_eq!(out.status.code(), Some(1), "no session to default to outside Silo");

    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// `silo config` with no server: path, init, check.
#[tokio::test(flavor = "multi_thread")]
async fn config_path_init_and_check() {
    let home = PathBuf::from(format!("/tmp/silo-cli-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join("config/init.lua");
    let shown = format!("silo-cli-config-{}/config/init.lua", std::process::id());

    let out = run(&home, &["config", "path"]).await;
    assert!(text(&out.stdout).trim().ends_with(&shown), "{}", text(&out.stdout));
    let out = run(&home, &["config", "check"]).await;
    assert!(out.status.success() && text(&out.stdout).contains("exists:   no"), "{}", text(&out.stdout));

    let out = run(&home, &["config", "init"]).await;
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).trim().ends_with(&shown), "{}", text(&out.stdout));
    assert!(path.is_file());
    let out = run(&home, &["config", "init"]).await;
    assert_eq!(out.status.code(), Some(1), "never overwrites");
    let out = run(&home, &["config", "check"]).await;
    let report = text(&out.stdout);
    assert!(out.status.success(), "{report}");
    assert!(report.contains("exists:   yes") && report.contains("1 action") && report.contains("1 handler"), "{report}");
    assert!(!report.contains("warning") && !report.contains("error"), "{report}");

    // A good file with a warning, and a bad one.
    let good = home.join("good.lua");
    std::fs::write(&good, "silo.mode('resize', { exclusive = true })\nsilo.action{ name = 'x', title = 'X', run = function() end }\nsilo.action{ name = 'x', title = 'X again', run = function() end }\n").unwrap();
    let out = run(&home, &["config", "check", good.to_str().unwrap()]).await;
    let report = text(&out.stdout);
    assert!(out.status.success(), "{report}");
    assert!(report.contains("1 mode") && report.contains("1 action") && report.contains("warning good.lua:3:"), "{report}");
    let bad = home.join("bad.lua");
    std::fs::write(&bad, "silo.mode('resize')\nlocal = 1\n").unwrap();
    let out = run(&home, &["config", "check", bad.to_str().unwrap()]).await;
    let report = text(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{report}");
    assert!(report.lines().any(|l| l.starts_with("error bad.lua:2:")), "{report}");
    let out = run(&home, &["config", "check", bad.to_str().unwrap(), "--json"]).await;
    assert_eq!(out.status.code(), Some(1));
    let state: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((state["failed"].clone(), state["problems"][0]["level"].clone()), (json!(true), json!("error")), "{state}");
    let out = run(&home, &["config", "check", home.join("missing.lua").to_str().unwrap()]).await;
    assert_eq!(out.status.code(), Some(1));

    let _ = std::fs::remove_dir_all(&home);
}
