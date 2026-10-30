use super::{FakeState, Inner, Req, form, read, respond};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpStream};

impl Inner {
    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn push(&self, message: Value) {
        let mut state = self.state();
        let id = state.log.last().map_or(1, |(id, _)| id + 1);
        state.log.push((id, message.to_string()));
        let _ = self.live.send((id, message.to_string()));
    }

    pub(crate) async fn handle(self: Arc<Self>, mut stream: TcpStream) {
        let Some(req) = read(&mut stream).await else { return };
        let json = |v: Value| (200, vec![("Content-Type", "application/json".to_owned())], v.to_string());
        let (status, headers, body) = match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/.well-known/oauth-protected-resource/mcp") if self.state().oauth => {
                json(json!({"resource":format!("{}/mcp", self.base),"authorization_servers":[self.base],"scopes_supported":["mcp"]}))
            }
            ("GET", "/.well-known/oauth-authorization-server") if self.state().oauth => json(json!({"issuer":self.base,
                "authorization_endpoint":format!("{}/authorize", self.base),"token_endpoint":format!("{}/token", self.base),
                "registration_endpoint":format!("{}/register", self.base),"code_challenge_methods_supported":["S256"]})),
            ("POST", "/register") => {
                let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
                self.state().registered.push(body);
                (201, vec![("Content-Type", "application/json".to_owned())], json!({"client_id":"client-1"}).to_string())
            }
            ("GET", "/authorize") => self.authorize(&req),
            ("POST", "/token") => self.token(&req),
            (_, "/mcp") if self.state().oauth && !self.authorized(&req) => {
                let challenge = format!(r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource/mcp", scope="mcp""#, self.base);
                (401, vec![("WWW-Authenticate", challenge)], String::new())
            }
            ("GET", "/mcp") => return self.stream(stream, req.headers.get("last-event-id").cloned()).await,
            ("DELETE", "/mcp") => (200, vec![], String::new()),
            ("POST", "/mcp") => match self.post(&req).await {
                Some(reply) => {
                    let sse = reply["result"]["content"].is_array();
                    let mut headers = vec![("Mcp-Session-Id", "session-1".to_owned())];
                    if sse {
                        headers.push(("Content-Type", "text/event-stream".to_owned()));
                        (200, headers, format!("event: message\ndata: {reply}\n\n"))
                    } else {
                        headers.push(("Content-Type", "application/json".to_owned()));
                        (200, headers, reply.to_string())
                    }
                }
                None => (202, vec![], String::new()),
            },
            _ => (404, vec![], String::new()),
        };
        respond(&mut stream, status, &headers, &body).await;
    }

    fn authorized(&self, req: &Req) -> bool {
        let token = req.headers.get("authorization").and_then(|v| v.strip_prefix("Bearer "));
        token.is_some_and(|t| self.state().tokens.contains(t))
    }

    fn authorize(&self, req: &Req) -> (u16, Vec<(&'static str, String)>, String) {
        let q = &req.query;
        let mut state = self.state();
        state.authorize.push(q.clone());
        if q.get("code_challenge_method").map(String::as_str) != Some("S256") || q.get("resource") != Some(&format!("{}/mcp", self.base)) {
            return (400, vec![], "bad authorization request".into());
        }
        state.issued += 1;
        let code = format!("code-{}", state.issued);
        state.codes.insert(code.clone(), (q["code_challenge"].clone(), q["redirect_uri"].clone()));
        let location = format!("{}?code={code}&state={}", q["redirect_uri"], q["state"]);
        (302, vec![("Location", location)], String::new())
    }

    fn token(&self, req: &Req) -> (u16, Vec<(&'static str, String)>, String) {
        let fields = form(&req.body);
        let mut state = self.state();
        state.token_requests.push(fields.clone());
        let ok = match fields.get("grant_type").map(String::as_str) {
            Some("authorization_code") => state.codes.remove(fields.get("code").map_or("", String::as_str)).is_some_and(|(challenge, redirect)| {
                let verifier = fields.get("code_verifier").map_or("", String::as_str);
                URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) == challenge && fields.get("redirect_uri") == Some(&redirect)
            }),
            Some("refresh_token") => fields.get("refresh_token").is_some_and(|r| state.refresh.remove(r)),
            _ => false,
        };
        if !ok || fields.get("resource") != Some(&format!("{}/mcp", self.base)) {
            return (400, vec![("Content-Type", "application/json".to_owned())], json!({"error":"invalid_grant"}).to_string());
        }
        state.issued += 1;
        let (access, refresh) = (format!("access-{}", state.issued), format!("refresh-{}", state.issued));
        state.tokens.insert(access.clone());
        state.refresh.insert(refresh.clone());
        let body = json!({"access_token":access,"refresh_token":refresh,"expires_in":3600,"token_type":"Bearer"});
        (200, vec![("Content-Type", "application/json".to_owned())], body.to_string())
    }

    /// The GET stream: replay after `last` (or what was never delivered), then live events.
    async fn stream(&self, mut stream: TcpStream, last: Option<String>) {
        let (mut live, mut kick) = (self.live.subscribe(), self.kick.subscribe());
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\nretry: 50\n\n";
        let replay: Vec<(u64, String)> = {
            let mut state = self.state();
            state.gets.push(last.clone());
            let after = last.and_then(|l| l.parse().ok()).unwrap_or(state.delivered);
            state.log.iter().filter(|(id, _)| *id > after).cloned().collect()
        };
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let mut sent = 0;
        for (id, data) in replay {
            sent = id;
            if !self.send(&mut stream, id, &data).await {
                return;
            }
        }
        loop {
            tokio::select! {
                event = live.recv() => match event {
                    Ok((id, data)) if id > sent => {
                        sent = id;
                        if !self.send(&mut stream, id, &data).await {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                },
                _ = kick.changed() => return,
            }
        }
    }

    async fn send(&self, stream: &mut TcpStream, id: u64, data: &str) -> bool {
        let ok = stream.write_all(format!("id: {id}\ndata: {data}\n\n").as_bytes()).await.is_ok();
        if ok {
            let mut state = self.state();
            state.delivered = state.delivered.max(id);
        }
        ok
    }

    /// Wait for the client's reply to server request `id`.
    pub(crate) async fn reply(&self, id: &str) -> Value {
        for _ in 0..100 {
            if let Some(reply) = self.state().responses.remove(id) {
                return reply;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        json!({"error":"no reply"})
    }
}
