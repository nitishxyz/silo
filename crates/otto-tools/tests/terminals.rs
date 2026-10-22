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
}

#[async_trait::async_trait]
impl Terminal for FakeTerminal {
    fn id(&self) -> &str {
        "b1"
    }
    async fn wait(&self, _: bool, screen: &mut (dyn for<'s> FnMut(&'s str) + Send)) -> Result<Finished, ToolError> {
        screen("live");
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
