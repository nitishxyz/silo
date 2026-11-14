//! The account's server's side of the login server
//! (`docs/LOGIN_SERVER.md` §6): who may say `role: login`, what that
//! principal reaches, `login.admit` for its own and another account, and
//! `login.revoke` leaving `silo pair` devices alone.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::Value;
use silo_client::Control;
use silo_proto::{
    Frame,
    access::{Rights, Scope, Who},
    control::{
        ClientHello, ClientRole, Created, GrantCreated, GrantKind, Grants, InvitePurpose, Method, PairInvite, Request, RpcError, ServerMessage,
        SpawnSpec,
    },
    kind,
    login::{LoginAdmitParams, LoginAdmitted, LoginRevokeParams},
    model::BlockKind,
    paths, read_frame, write_frame,
};
use silo_transport::{EndpointAddr, Identity, Net, PairTicket, RelayConfig};
use tokio::{net::UnixStream, task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-login-srv-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path, trust_own_login: bool) -> JoinHandle<()> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        relay: RelayConfig::Disabled,
        mdns: false,
        grant_sweep: Duration::from_millis(200),
        trust_own_login,
        ..Default::default()
    };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn shutdown(owner: Control, task: JoinHandle<()>) {
    owner.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(owner);
    timeout(T, task).await.unwrap().unwrap();
}

/// A unix connection speaking raw frames, so its hello says exactly what
/// the test wants (`role: login`).
struct Raw {
    stream: UnixStream,
    next: u64,
}

impl Raw {
    async fn hello(home: &Path, hello: &ClientHello) -> Result<Self, RpcError> {
        let mut stream = UnixStream::connect(paths::socket(home)).await.unwrap();
        write_frame(&mut stream, &Frame::json(kind::CONTROL_HELLO, hello).unwrap()).await.unwrap();
        let reply = timeout(T, read_frame(&mut stream)).await.unwrap().unwrap().expect("a reply to the hello");
        if reply.kind == kind::CONTROL_HELLO {
            return Ok(Self { stream, next: 1 });
        }
        match reply.parse::<ServerMessage>().unwrap() {
            ServerMessage::Response { error: Some(error), .. } => Err(error),
            other => panic!("unexpected reply {other:?}"),
        }
    }

    async fn login(home: &Path) -> Result<Self, RpcError> {
        Self::hello(home, &ClientHello { role: ClientRole::Login, ..ClientHello::ours("silo login server", true, None) }).await
    }

    async fn call(&mut self, method: Method) -> Result<Value, RpcError> {
        let id = self.next;
        self.next += 1;
        write_frame(&mut self.stream, &Frame::json(kind::CONTROL, &Request { id, method }).unwrap()).await.unwrap();
        loop {
            let frame = timeout(T, read_frame(&mut self.stream)).await.unwrap().unwrap().expect("a reply");
            if let Ok(ServerMessage::Response { id: got, result, error }) = frame.parse::<ServerMessage>()
                && got == id
            {
                return error.map_or_else(|| Ok(result.unwrap_or(Value::Null)), Err);
            }
        }
    }

    async fn admit(&mut self, device: &Identity, account: &str) -> Result<LoginAdmitted, RpcError> {
        let params = LoginAdmitParams { device: device.node_id(), name: "phone".into(), account: account.into() };
        self.call(Method::LoginAdmit(params)).await.map(|v| serde_json::from_value(v).unwrap())
    }

    async fn revoke(&mut self, device: &Identity, account: Option<&str>) {
        let params = LoginRevokeParams { device: device.node_id(), account: account.map(Into::into) };
        self.call(Method::LoginRevoke(params)).await.unwrap();
    }
}

fn own() -> String {
    silo_server::login::own_account().expect("this process's account has a name")
}

/// `addr` with only its IPv4 port, on loopback.
fn loopback(addr: &EndpointAddr) -> EndpointAddr {
    let port = addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port).expect("an IPv4 address");
    EndpointAddr::new(addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into())
}

async fn grants(owner: &Control) -> Vec<silo_proto::access::Grant> {
    owner.call::<Grants>(Method::GrantList {}).await.unwrap().grants
}

fn shell() -> SpawnSpec {
    SpawnSpec { cwd: None, command: vec!["/bin/sh".into()], size: Some(silo_proto::model::TermSize::new(80, 24)) }
}

#[tokio::test(flavor = "multi_thread")]
async fn role_login_from_a_non_root_peer_is_refused_without_the_test_knob() {
    if rustix::process::getuid().is_root() {
        return; // uid 0 is the login server's own uid: admitted by design.
    }
    let home = temp_home("refused");
    let task = start(&home, false).await;
    let refused = Raw::login(&home).await.err().expect("refused");
    assert_eq!(refused.code, RpcError::UNAUTHORIZED);
    let (owner, _) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    shutdown(owner, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_login_server_reaches_only_login_methods_server_info_and_ping() {
    let home = temp_home("principal");
    let task = start(&home, true).await;
    let mut login = Raw::login(&home).await.expect("admitted as the login server");
    login.call(Method::Ping {}).await.unwrap();
    login.call(Method::ServerInfo {}).await.unwrap();
    for method in [
        Method::SessionList {},
        Method::GrantList {},
        Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None },
        Method::PairCreate { ttl_secs: 60, purpose: InvitePurpose::Device },
        Method::ApiList {},
    ] {
        let error = login.call(method.clone()).await.unwrap_err();
        assert_eq!(error.code, RpcError::FORBIDDEN, "{method:?}");
    }
    // And nobody else reaches login.*: not even the owner.
    let (owner, _) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let params = LoginAdmitParams { device: Identity::ephemeral().node_id(), name: String::new(), account: own() };
    let error = owner.call_value(Method::LoginAdmit(params)).await.unwrap_err().downcast::<RpcError>().unwrap();
    assert_eq!(error.code, RpcError::FORBIDDEN);
    shutdown(owner, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn own_account_admits_a_device_and_revoke_leaves_paired_devices_alone() {
    let home = temp_home("own");
    let task = start(&home, true).await;
    let (owner, _) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut login = Raw::login(&home).await.unwrap();

    // A device paired with `silo pair`.
    let paired = Identity::ephemeral();
    let paired_net = Net::bind(&paired, RelayConfig::Disabled, false).await.unwrap();
    let invite: PairInvite = owner.call(Method::PairCreate { ttl_secs: 60, purpose: InvitePurpose::Device }).await.unwrap();
    let pair = PairTicket::decode(&invite.ticket).unwrap();
    let pair = PairTicket { addr: loopback(&pair.addr), ..pair };
    drop(silo_client::pair(&paired_net, &pair, "paired").await.unwrap());

    // A device the login server admits.
    let phone = Identity::ephemeral();
    let admitted = login.admit(&phone, &own()).await.unwrap();
    let addr = loopback(&silo_transport::decode_addr(&admitted.ticket).unwrap());
    assert_eq!(login.admit(&phone, &own()).await.unwrap().grant_id, admitted.grant_id, "admitting again finds it");
    let grant = grants(&owner).await.into_iter().find(|g| g.id == admitted.grant_id).unwrap();
    assert_eq!((grant.who, grant.scope, grant.rights), (Who::Device { id: phone.node_id() }, Scope::Server, Rights::Control));
    assert_eq!((grant.label.as_str(), grant.created_by.as_str()), ("phone", silo_server::login::LOGIN_CREATOR));
    // Admitting the paired device finds its pairing; nothing new.
    let found = login.admit(&paired, &own()).await.unwrap();
    assert_eq!(grants(&owner).await.iter().find(|g| g.id == found.grant_id).unwrap().created_by, "");

    let phone_net = Net::bind(&phone, RelayConfig::Disabled, false).await.unwrap();
    let (control, _) = Control::connect_remote(&phone_net, addr.clone(), "phone", false, None).await.unwrap();
    control.call_value(Method::SessionList {}).await.unwrap();

    // `login.revoke` takes the login server's device and closes it…
    login.revoke(&paired, None).await;
    login.revoke(&phone, Some(&own())).await;
    timeout(T, control.closed()).await.expect("the revoked device's connection closes");
    let left = grants(&owner).await;
    assert!(left.iter().all(|g| g.id != admitted.grant_id));
    // …and never a paired one.
    assert!(left.iter().any(|g| g.who == Who::Device { id: paired.node_id() }), "the paired device stays");
    let (paired_control, _) = Control::connect_remote(&paired_net, addr, "paired", false, None).await.unwrap();
    paired_control.call_value(Method::SessionList {}).await.unwrap();

    drop((paired_control, control));
    paired_net.close().await.unwrap();
    phone_net.close().await.unwrap();
    shutdown(owner, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_account_grant_admits_a_device_with_its_scope_and_takes_it_away() {
    let home = temp_home("account");
    let task = start(&home, true).await;
    let (owner, _) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let mut login = Raw::login(&home).await.unwrap();
    let created: Created =
        owner.call(Method::SessionCreate { name: Some("work".into()), spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let scope = Scope::Session { session_id: created.session_id.clone() };
    let account = |label: &str, rights| Method::GrantCreate {
        scope: scope.clone(),
        rights,
        kind: GrantKind::Account,
        ttl_secs: None,
        label: Some(label.into()),
    };

    // Account grants: a user name, never owner.
    for (label, rights) in [("bad name", Rights::View), ("", Rights::View), ("ana", Rights::Owner)] {
        let error = owner.call_value(account(label, rights)).await.unwrap_err().downcast::<RpcError>().unwrap();
        assert_eq!(error.code, "bad_request", "{label:?} {rights:?}");
    }
    // Without one, another account gets nothing.
    let phone = Identity::ephemeral();
    assert_eq!(login.admit(&phone, "ana").await.unwrap_err().code, RpcError::FORBIDDEN);

    let granted: GrantCreated = owner.call(account("ana", Rights::View)).await.unwrap();
    assert_eq!(granted.grant.who, Who::Account { name: "ana".into() });
    let admitted = login.admit(&phone, "ana").await.unwrap();
    let grant = grants(&owner).await.into_iter().find(|g| g.id == admitted.grant_id).unwrap();
    assert_eq!((grant.scope.clone(), grant.rights), (scope.clone(), Rights::View), "the account grant's scope and rights");
    assert_eq!(grant.created_by_grant.as_deref(), Some(granted.grant.id.as_str()));

    // The device sees what the grant covers, as a share link does.
    let phone_net = Net::bind(&phone, RelayConfig::Disabled, false).await.unwrap();
    let addr = loopback(&silo_transport::decode_addr(&admitted.ticket).unwrap());
    let (control, _) = Control::connect_remote(&phone_net, addr.clone(), "phone", false, None).await.unwrap();
    let granted_hello = control.hello().granted.clone().expect("its access is in the hello");
    assert_eq!((granted_hello.scope, granted_hello.rights), (scope.clone(), Rights::View));
    control.call_value(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    let error = control
        .call_value(Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None })
        .await
        .unwrap_err()
        .downcast::<RpcError>()
        .unwrap();
    assert_eq!(error.code, RpcError::FORBIDDEN);

    // Revoking the account grant takes the device with it.
    owner.call_value(Method::GrantRevoke { id: granted.grant.id.clone() }).await.unwrap();
    timeout(T, control.closed()).await.expect("the device's connection closes with the account grant");
    assert!(grants(&owner).await.iter().all(|g| g.id != admitted.grant_id), "its device grant is gone");
    assert!(Control::connect_remote(&phone_net, addr, "phone", false, None).await.is_err());

    drop(control);
    phone_net.close().await.unwrap();
    shutdown(owner, task).await;
    let _ = std::fs::remove_dir_all(&home);
}
