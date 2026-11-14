//! The login server end to end, unprivileged: a broker in this process
//! whose spawner starts an in-process account server in a temp home.
//! invite → the device is written into the accounts file → `login.admit`
//! → the device connects to the account's server as a paired device.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use silo_client::{Control, login::LoginRefused};
use silo_login::{
    Broker, code,
    policy::Policy,
    spawner::Spawner,
    users::{self, User, Users},
};
use silo_proto::{control::Method, model::SessionSummary, paths};
use silo_transport::{EndpointAddr, Identity, LoginTicket, Net, RelayConfig};
use tokio::time::timeout;

const T: Duration = Duration::from_secs(20);

fn temp(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-login-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// This process's account, at home in `home`.
struct Me(User);

impl Users for Me {
    fn user(&self, name: &str) -> Option<User> {
        (name == self.0.name).then(|| self.0.clone())
    }
    fn locked(&self, _: &str) -> bool {
        false
    }
    fn nologin(&self) -> bool {
        false
    }
}

/// Starts the account's server in this process, trusting `role: login`
/// from its own uid (what `--unprivileged` tells a real one).
struct InProcess {
    home: PathBuf,
    started: AtomicUsize,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Spawner for InProcess {
    fn socket(&self, _: &User) -> PathBuf {
        paths::socket(&self.home)
    }

    fn start(&self, _: &User) -> anyhow::Result<()> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let config = silo_server::Config {
            home: self.home.clone(),
            idle_exit: None,
            relay: RelayConfig::Disabled,
            mdns: false,
            trust_own_login: true,
            ..Default::default()
        };
        *self.task.lock().unwrap() = Some(tokio::spawn(async move { silo_server::run(config).await.unwrap() }));
        Ok(())
    }
}

/// `addr` with only its IPv4 port, on loopback.
fn loopback(addr: &EndpointAddr) -> EndpointAddr {
    let port = addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port).expect("an IPv4 address");
    EndpointAddr::new(addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into())
}

async fn bind(identity: &Identity) -> Net {
    Net::bind(identity, RelayConfig::Disabled, false).await.unwrap()
}

/// The login server's endpoint, serving `broker`; its loopback address.
async fn serve(broker: &Arc<Broker>) -> (Arc<Net>, EndpointAddr) {
    let mut net = bind(&Identity::ephemeral()).await;
    let incoming = net.serve_login(8).unwrap();
    tokio::spawn(broker.clone().serve(incoming));
    let port = net.bound_sockets().into_iter().find(SocketAddr::is_ipv4).expect("an IPv4 socket").port();
    let addr = EndpointAddr::new(net.id()).with_ip_addr((Ipv4Addr::LOCALHOST, port).into());
    (Arc::new(net), addr)
}

fn refused(error: &anyhow::Error) -> String {
    error.downcast_ref::<LoginRefused>().map_or_else(|| format!("{error:#}"), |r| r.code.clone())
}

async fn shutdown(home: &Path, spawner: &InProcess) {
    if let Ok((owner, _)) = Control::connect(&paths::socket(home), "login-test").await {
        let _ = owner.call_value(Method::ServerShutdown { force: false }).await;
    }
    let task = spawner.task.lock().unwrap().take();
    if let Some(task) = task {
        let _ = timeout(T, task).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invited_device_logs_in_and_connects_as_a_paired_device() {
    let dir = temp("e2e");
    let home = dir.join("home");
    let accounts = dir.join("accounts.json");
    let me = users::current().expect("this process's account");
    let account = me.name.clone();
    let spawner = Arc::new(InProcess { home: home.clone(), started: AtomicUsize::new(0), task: Mutex::new(None) });
    let broker = Arc::new(
        Broker::new(
            accounts.clone(),
            Arc::new(Me(User { home: home.clone(), shell: "/bin/sh".into(), ..me })),
            spawner.clone(),
            Some(account.clone()),
        )
        .unwrap(),
    );
    let (_login_net, login_addr) = serve(&broker).await;

    // The invite: single use, for this account.
    let ticket = LoginTicket::parse(&broker.invite_ticket(login_addr.clone(), &account, Duration::from_secs(3600)).unwrap()).unwrap();
    assert_eq!(ticket.account.as_deref(), Some(account.as_str()));
    let laptop = Identity::ephemeral();
    let laptop_net = bind(&laptop).await;
    let reply = timeout(T, silo_client::login::login(&laptop_net, &ticket, None, None, "laptop")).await.unwrap().unwrap();
    assert_eq!(reply.account, account);
    assert_eq!(spawner.started.load(Ordering::SeqCst), 1, "the account's server was started");

    // The device is in the file now, with its name.
    let policy = Policy::load(&accounts).unwrap();
    let listed = &policy.accounts()[&account];
    assert!(listed.iter().any(|d| d.id == laptop.node_id() && d.name == "laptop"), "{listed:?}");

    // The invite was used up.
    let phone_net = bind(&Identity::ephemeral()).await;
    let again = silo_client::login::login(&phone_net, &ticket, None, None, "phone").await.unwrap_err();
    assert_eq!(refused(&again), code::INVITE);
    // A device not in the file gets nothing without an invite.
    let plain = LoginTicket { secret: None, ..ticket.clone() };
    assert_eq!(refused(&silo_client::login::login(&phone_net, &plain, None, None, "phone").await.unwrap_err()), code::NOT_ALLOWED);

    // Later logins need no invite and find the server running.
    let reply = timeout(T, silo_client::login::login(&laptop_net, &plain, None, None, "laptop")).await.unwrap().unwrap();
    assert_eq!(spawner.started.load(Ordering::SeqCst), 1, "not started twice");

    // The device dials the account's server as a paired device.
    let addr = loopback(&silo_client::login::server_addr(&reply).unwrap());
    assert_eq!(addr.id.to_string(), reply.server_id);
    let (control, _events) = timeout(T, Control::connect_remote(&laptop_net, addr.clone(), "laptop", false, None)).await.unwrap().unwrap();
    assert!(control.hello().granted.is_none(), "its own account: a plain paired device");
    let _: Vec<SessionSummary> = control.call(Method::SessionList {}).await.unwrap();

    // Denied: its server revokes it and its connection closes.
    assert!(broker.deny(&account, &laptop.node_id()).await.unwrap());
    timeout(T, control.closed()).await.expect("a denied device's connection closes");
    assert_eq!(refused(&silo_client::login::login(&laptop_net, &plain, None, None, "laptop").await.unwrap_err()), code::NOT_ALLOWED);
    assert!(Control::connect_remote(&laptop_net, addr, "laptop", false, None).await.is_err(), "no longer paired there");

    laptop_net.close().await.unwrap();
    phone_net.close().await.unwrap();
    shutdown(&home, &spawner).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_attempts_are_rate_limited_and_other_accounts_refused() {
    let dir = temp("limits");
    let home = dir.join("home");
    let me = users::current().expect("this process's account");
    let account = me.name.clone();
    let spawner = Arc::new(InProcess { home: home.clone(), started: AtomicUsize::new(0), task: Mutex::new(None) });
    let broker = Arc::new(
        Broker::new(
            dir.join("accounts.json"),
            Arc::new(Me(User { home: home.clone(), shell: "/bin/sh".into(), ..me })),
            spawner.clone(),
            Some(account.clone()),
        )
        .unwrap(),
    );
    let (_login_net, login_addr) = serve(&broker).await;
    // An unprivileged login server serves only its own account.
    assert!(broker.invite("someone_else", Duration::from_secs(60)).is_err());
    assert!(broker.invite("bad name", Duration::from_secs(60)).is_err());
    assert!(broker.invite(&account, Duration::ZERO).is_err());

    let stranger = bind(&Identity::ephemeral()).await;
    let ticket = LoginTicket { addr: login_addr, secret: Some([9; 32]), account: None };
    for _ in 0..5 {
        assert_eq!(refused(&silo_client::login::login(&stranger, &ticket, None, None, "x").await.unwrap_err()), code::INVITE);
    }
    assert_eq!(refused(&silo_client::login::login(&stranger, &ticket, None, None, "x").await.unwrap_err()), code::RATE_LIMITED);
    assert_eq!(spawner.started.load(Ordering::SeqCst), 0, "nothing was started for a refused device");
    stranger.close().await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// This process's account and `ana`, another account whose devices log
/// in only to servers shared with her by name.
struct MeAndAna(User);

impl Users for MeAndAna {
    fn user(&self, name: &str) -> Option<User> {
        match name {
            _ if name == self.0.name => Some(self.0.clone()),
            ANA => Some(User { name: ANA.into(), uid: self.0.uid + 1, home: self.0.home.join("ana"), ..self.0.clone() }),
            _ => None,
        }
    }
    fn locked(&self, _: &str) -> bool {
        false
    }
    fn nologin(&self) -> bool {
        false
    }
}

const ANA: &str = "silo_test_ana";

#[tokio::test(flavor = "multi_thread")]
async fn server_of_reaches_another_accounts_server_through_its_account_grant() {
    use silo_proto::{
        access::{Rights, Scope},
        control::{Created, GrantKind, SpawnSpec},
        model::BlockKind,
    };
    let dir = temp("server-of");
    let home = dir.join("home");
    let me = users::current().expect("this process's account");
    assert_ne!(me.name, ANA);
    let owner_account = me.name.clone();
    let spawner = Arc::new(InProcess { home: home.clone(), started: AtomicUsize::new(0), task: Mutex::new(None) });
    // Not `only`: it serves two accounts, both started in this process.
    let users = Arc::new(MeAndAna(User { home: home.clone(), shell: "/bin/sh".into(), ..me }));
    let broker = Arc::new(Broker::new(dir.join("accounts.json"), users, spawner.clone(), None).unwrap());
    let (_login_net, login_addr) = serve(&broker).await;

    // Ana's phone may use her account (not the owner's).
    let phone = Identity::ephemeral();
    assert!(broker.allow(ANA, &phone.node_id()).await.unwrap());
    let phone_net = bind(&phone).await;
    let ticket = LoginTicket { addr: login_addr, secret: None, account: Some(ANA.into()) };
    let login = |server_of| silo_client::login::login(&phone_net, &ticket, None, server_of, "phone");

    // Nothing shared with her yet: refused (and the owner's server started).
    assert_eq!(refused(&timeout(T, login(Some(&owner_account))).await.unwrap().unwrap_err()), code::NOT_ALLOWED);
    assert_eq!(spawner.started.load(Ordering::SeqCst), 1);
    let bad = timeout(T, login(Some("bad name"))).await.unwrap().unwrap_err();
    assert_eq!(refused(&bad), code::BAD_REQUEST);

    // The owner shares a session with her by name.
    let (owner, _) = Control::connect(&paths::socket(&home), "owner").await.unwrap();
    let shell = SpawnSpec { cwd: None, command: vec!["/bin/sh".into()], size: Some(silo_proto::model::TermSize::new(80, 24)) };
    let created: Created =
        owner.call(Method::SessionCreate { name: Some("work".into()), spawn: shell, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let scope = Scope::Session { session_id: created.session_id.clone() };
    owner
        .call_value(Method::GrantCreate {
            scope: scope.clone(),
            rights: Rights::View,
            kind: GrantKind::Account,
            ttl_secs: None,
            label: Some(ANA.into()),
        })
        .await
        .unwrap();

    // Logged in as ana, for the owner's server: she sees what it covers.
    let reply = timeout(T, login(Some(&owner_account))).await.unwrap().unwrap();
    assert_eq!(reply.account, ANA, "logged in as herself");
    let addr = loopback(&silo_client::login::server_addr(&reply).unwrap());
    let (control, _events) = timeout(T, Control::connect_remote(&phone_net, addr, "phone", false, None)).await.unwrap().unwrap();
    let granted = control.hello().granted.clone().expect("a device admitted by an account grant");
    assert_eq!((granted.scope, granted.rights), (scope, Rights::View));
    assert_eq!(spawner.started.load(Ordering::SeqCst), 1, "not started twice");

    drop(control);
    drop(owner);
    phone_net.close().await.unwrap();
    shutdown(&home, &spawner).await;
    let _ = std::fs::remove_dir_all(&dir);
}
