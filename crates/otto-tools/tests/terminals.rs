//! `shell` in a terminal host (silo's command blocks), with a fake host.

use otto_tools::{
    Mailbox, Tool, ToolCx, ToolError,
    builtin::shell::{Finished, Host, Jobs, Shell, Terminal, Terminals},
};
use serde_json::json;
use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Fake {
    started: Mutex<Vec<String>>,
    script: Mutex<VecDeque<Finished>>,
    stopped: Mutex<bool>,
    /// The first wait holds until released (a command still running).
    hold: Option<Arc<tokio::sync::Notify>>,
    held: std::sync::atomic::AtomicBool,
}

struct FakeTerminals(Arc<Fake>);
struct FakeTerminal(Arc<Fake>);

#[async_trait::async_trait]
impl Terminals for FakeTerminals {
    async fn start(&self, command: &str, _: &Path, env: &[(&str, &str)], _: &ToolCx) -> Result<Option<Arc<dyn Terminal>>, ToolError> {
        assert!(env.contains(&("PAGER", "cat")));
        self.0.started.lock().unwrap().push(command.into());
        Ok(Some(Arc::new(FakeTerminal(self.0.clone()))))
    }
    async fn attach(&self, block: &str, host: &str, _: &ToolCx) -> Option<Arc<dyn Terminal>> {
        (block == "b1" && host == "fake").then(|| Arc::new(FakeTerminal(self.0.clone())) as Arc<dyn Terminal>)
    }
}

#[async_trait::async_trait]
impl Terminal for FakeTerminal {
    fn id(&self) -> &str {
        "b1"
    }
    fn host(&self) -> &str {
        "fake"
    }
    async fn wait(&self, _: bool, screen: &mut (dyn for<'s> FnMut(&'s str) + Send)) -> Result<Finished, ToolError> {
        screen("live");
        if let Some(hold) = &self.0.hold
            && !self.0.held.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            hold.notified().await;
        }
        let next = self.0.script.lock().unwrap().pop_front();
        match next {
            Some(finished) => Ok(finished),
            None => std::future::pending().await,
        }
    }
    async fn stop(&self) -> Result<String, ToolError> {
        *self.0.stopped.lock().unwrap() = true;
        Ok("partial".into())
    }
}

#[derive(Default)]
struct Mail(Mutex<Vec<String>>);
impl Mailbox for Mail {
    fn deliver(&self, text: String) {
        self.0.lock().unwrap().push(text);
    }
}

fn setup(script: Vec<Finished>) -> (Shell, Arc<Fake>, Arc<Mail>, ToolCx, tempfile::TempDir) {
    let fake = Arc::new(Fake { script: Mutex::new(script.into()), ..Fake::default() });
    let shell = Shell::with(Host { jobs: Arc::new(Jobs::default()), terminals: Some(Arc::new(FakeTerminals(fake.clone()))) });
    let dir = tempfile::tempdir().unwrap();
    let mail = Arc::new(Mail::default());
    let mut cx = ToolCx::new("t", dir.path());
    cx.spill_dir = Some(dir.path().join("spill"));
    cx.sessions = vec!["ses".into()];
    cx.mailbox = Some(mail.clone());
    (shell, fake, mail, cx, dir)
}

async fn delivered(mail: &Mail) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(text) = mail.0.lock().unwrap().pop() {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn quick_commands_use_pipes_others_a_terminal() {
    let (shell, fake, _, cx, _dir) = setup(vec![Finished::Exited { code: Some(3), text: "boom".into() }]);
    assert_eq!(shell.call(json!({"command":"echo hi"}), cx.clone()).await.unwrap().joined_text(), "hi\n");
    let out = shell.call(json!({"command":"cargo test"}), cx).await.unwrap();
    assert_eq!(*fake.started.lock().unwrap(), ["cargo test"]);
    assert!(out.is_error);
    assert_eq!((out.metadata["exit_code"].clone(), out.metadata["block_id"].clone()), (json!(3), json!("b1")));
    assert_eq!(out.joined_text(), "boom\n[exit code 3]");
}

#[tokio::test]
async fn closing_the_block_stops_the_command() {
    let (shell, _, mail, cx, _dir) = setup(vec![Finished::Closed, Finished::Closed]);
    let out = shell.call(json!({"command":"cargo test"}), cx.clone()).await.unwrap();
    assert_eq!((out.metadata["termination"].clone(), out.joined_text()), (json!("closed"), "[stopped: closed]".to_owned()));
    shell.call(json!({"command":"cargo test","background":true}), cx).await.unwrap();
    assert!(delivered(&mail).await.contains("was stopped (closed)."));
}

#[tokio::test]
async fn a_prompt_returns_waiting_and_the_result_arrives_later() {
    let script = vec![Finished::Waiting { screen: "Password:".into() }, Finished::Exited { code: Some(0), text: "done".into() }];
    let (shell, _, mail, cx, _dir) = setup(script);
    let out = shell.call(json!({"command":"sudo true"}), cx).await.unwrap();
    assert_eq!(out.metadata["status"], "waiting_for_input");
    assert_eq!(out.metadata["block_id"], "b1");
    assert!(out.joined_text().starts_with("waiting_for_input") && out.joined_text().ends_with("Password:"));
    let job = out.metadata["job_id"].as_str().unwrap().to_owned();
    assert_eq!(delivered(&mail).await, format!("Background shell {job} (`sudo true`) in block b1 exited with code 0.\ndone"));
}

#[tokio::test]
async fn background_commands_report_and_timeouts_stop() {
    let (shell, fake, mail, cx, _dir) = setup(vec![Finished::Exited { code: Some(0), text: "ok".into() }]);
    let out = shell.call(json!({"command":"cargo build","background":true}), cx.clone()).await.unwrap();
    assert_eq!((out.metadata["background"].clone(), out.metadata["block_id"].clone()), (json!(true), json!("b1")));
    assert!(delivered(&mail).await.ends_with("exited with code 0.\nok"));
    assert!(out.joined_text().starts_with("Started ") && out.joined_text().contains(" in block b1;"));
    let out = shell.call(json!({"command":"cargo run","timeout_ms":50}), cx).await.unwrap();
    assert!(*fake.stopped.lock().unwrap());
    assert_eq!(out.metadata["termination"], "timeout");
    assert_eq!(out.joined_text(), "partial\n[stopped: timeout]");
}

/// Detach `call` of session `ses` once it runs.
async fn detach(jobs: &Jobs, call: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !jobs.detach("ses", call) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_detached_terminal_command_runs_on_as_a_job() {
    let fake = Arc::new(Fake {
        script: Mutex::new(vec![Finished::Exited { code: Some(0), text: "passed".into() }].into()),
        hold: Some(Arc::new(tokio::sync::Notify::new())),
        ..Fake::default()
    });
    let jobs = Arc::new(Jobs::default());
    let shell = Shell::with(Host { jobs: jobs.clone(), terminals: Some(Arc::new(FakeTerminals(fake.clone()))) });
    let (_, _, mail, cx, _dir) = setup(vec![]);
    assert!(!jobs.detach("ses", "t"), "nothing runs yet");
    let call = shell.call(json!({"command":"gh run watch"}), cx);
    let (out, ()) = tokio::join!(call, detach(&jobs, "t"));
    let out = out.unwrap();
    let id = out.metadata["job_id"].as_str().unwrap().to_owned();
    assert_eq!((out.metadata["detached"].clone(), out.metadata["block_id"].clone()), (json!(true), json!("b1")));
    assert!(out.joined_text().starts_with(&format!("Started {id} in block b1: the user detached")));
    assert!(!*fake.stopped.lock().unwrap(), "detaching does not stop it");
    assert_eq!(delivered(&mail).await, format!("Background shell {id} (`gh run watch`) in block b1 exited with code 0.\npassed"));
    assert!(!jobs.detach("ses", "t"), "a returned call is no longer detachable");
}

#[tokio::test]
async fn a_detached_pipe_command_reports_and_is_listed_until_killed() {
    let jobs = Arc::new(Jobs::default());
    let shell = Shell::new(jobs.clone());
    let (_, _, mail, mut cx, _dir) = setup(vec![]);
    let (tx, mut progress) = tokio::sync::mpsc::unbounded_channel();
    cx.progress = Some(tx);
    let call = shell.call(json!({"command":"printf early; sleep 0.3; printf late"}), cx.clone());
    let (out, ()) = tokio::join!(call, detach(&jobs, "t"));
    let out = out.unwrap();
    let id = out.metadata["job_id"].as_str().unwrap().to_owned();
    assert!(out.joined_text().starts_with(&format!("Started {id}: the user detached")));
    let listed = jobs.list("ses").await;
    assert_eq!((listed.len(), listed[0].job_id.as_str(), listed[0].call_id.as_str(), listed[0].completed), (1, id.as_str(), "t", false));
    assert!(jobs.list("other").await.is_empty());
    while progress.try_recv().is_ok() {}
    // It ran on: its whole output arrives, and the returned call gets no more progress.
    assert_eq!(delivered(&mail).await, format!("Background shell {id} (`printf early; sleep 0.3; printf late`) exited with code 0.\nearlylate"));
    assert!(progress.try_recv().is_err());
    assert!(jobs.list("ses").await[0].completed);
    // A running one is stopped by `kill` (only its own session's).
    cx.call_id = "t2".into();
    let call = shell.call(json!({"command":"sleep 10"}), cx);
    let (out, ()) = tokio::join!(call, detach(&jobs, "t2"));
    let id = out.unwrap().metadata["job_id"].as_str().unwrap().to_owned();
    assert!(!jobs.kill("other", &id).await);
    assert!(jobs.kill("ses", &id).await);
    assert!(delivered(&mail).await.contains("was stopped (cancelled)"));
}

#[tokio::test]
async fn a_foreground_command_stops_when_its_call_is_dropped() {
    let (_, _, _, cx, dir) = setup(vec![]);
    let shell = Shell::default();
    let marker = dir.path().join("marker");
    let command = format!("sleep 0.3; touch {}", marker.display());
    assert!(tokio::time::timeout(Duration::from_millis(50), shell.call(json!({"command": command}), cx)).await.is_err());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!marker.exists(), "the command ran on after its call was dropped");
}

#[tokio::test]
async fn a_job_whose_block_lives_on_is_resumed_after_a_restart() {
    let (shell, _, mail, cx, _dir) = setup(vec![]);
    let out = shell.call(json!({"command":"cargo build","background":true}), cx.clone()).await.unwrap();
    assert_eq!((out.metadata["block_id"].clone(), out.metadata["host"].clone()), (json!("b1"), json!("fake")), "kept with the call");
    // A new process: the same host (its block runs on), new jobs.
    let fake = Arc::new(Fake { script: Mutex::new(vec![Finished::Exited { code: Some(0), text: "ok".into() }].into()), ..Fake::default() });
    let jobs = Arc::new(Jobs::default());
    let host = Host { jobs: jobs.clone(), terminals: Some(Arc::new(FakeTerminals(fake.clone()))) };
    let job = |block: &str, host: &str| otto_tools::builtin::shell::Resumed {
        job_id: "job-1".into(),
        command: "cargo build".into(),
        block_id: block.into(),
        host: host.into(),
        started_at: 7,
        timeout: None,
        prompt: true,
    };
    assert!(!host.resume(job("gone", "fake"), cx.clone()).await.unwrap(), "its block is gone");
    assert!(!host.resume(job("b1", "other"), cx.clone()).await.unwrap(), "another host's");
    assert!(!Host::default().resume(job("b1", "fake"), cx.clone()).await.unwrap(), "no terminal host");
    while mail.0.lock().unwrap().pop().is_some() {}
    assert!(host.resume(job("b1", "fake"), cx).await.unwrap());
    let listed = jobs.list("ses").await;
    assert_eq!((listed[0].job_id.as_str(), listed[0].block_id.as_deref(), listed[0].started_at), ("job-1", Some("b1"), 7));
    assert_eq!(delivered(&mail).await, "Background shell job-1 (`cargo build`) in block b1 exited with code 0.\nok");
}
