//! Paired remote hosts, stored in `<home>/hosts.json` (shared by the CLI
//! and the app, as is the device key in `<home>/client`), keyed by the
//! host's canonical EndpointId.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use silo_proto::control::{Method, ServerInfo};
use silo_transport::EndpointAddr;

use crate::Control;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    /// Canonical EndpointId string.
    pub id: String,
    pub name: String,
    pub addr: EndpointAddr,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hosts {
    pub hosts: Vec<Host>,
}

/// Where the device key lives.
#[must_use]
pub fn client_dir(home: &Path) -> PathBuf {
    home.join("client")
}

/// The connected server's current address from `server.info`, if it
/// reports one.
///
/// # Errors
/// RPC failure or an undecodable address.
pub async fn current_addr(control: &Control) -> anyhow::Result<Option<EndpointAddr>> {
    let info: ServerInfo = control.call(Method::ServerInfo {}).await?;
    info.endpoint_addr.map(serde_json::from_value).transpose().context("decode server endpoint_addr")
}

/// Update the stored address of the connected host when it moved. Hosts
/// not in `hosts.json` (an unsaved ticket) are left alone.
///
/// # Errors
/// RPC failure or `hosts.json` I/O.
pub async fn refresh_host(home: &Path, control: &Control) -> anyhow::Result<()> {
    match current_addr(control).await? {
        Some(addr) => store_addr(home, addr),
        None => Ok(()),
    }
}

/// Store `addr` (a host's own report of its address) for the stored host
/// with its id when it moved; an address without any route is ignored.
///
/// # Errors
/// `hosts.json` I/O.
pub fn store_addr(home: &Path, addr: EndpointAddr) -> anyhow::Result<()> {
    if addr.is_empty() {
        return Ok(());
    }
    let id = addr.id.to_string();
    if Hosts::load(home)?.hosts.iter().any(|h| h.id == id && h.addr != addr) {
        Hosts::update(home, |hosts| {
            if hosts.hosts.iter().any(|h| h.id == id) {
                hosts.upsert(addr, None)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn file(home: &Path) -> PathBuf {
    home.join("hosts.json")
}

impl Hosts {
    /// # Errors
    /// Unreadable or invalid `hosts.json`.
    pub fn load(home: &Path) -> anyhow::Result<Self> {
        let path = file(home);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        }
    }

    /// Read-modify-write `hosts.json` under an exclusive lock on
    /// `hosts.json.lock` (shared with the app); written only when `change`
    /// succeeds, never over an unreadable file.
    ///
    /// # Errors
    /// Unreadable or invalid `hosts.json`, `change` failed, or the file
    /// cannot be written.
    pub fn update<T>(home: &Path, change: impl FnOnce(&mut Self) -> anyhow::Result<T>) -> anyhow::Result<T> {
        use std::os::{fd::AsFd, unix::fs::OpenOptionsExt};
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700).create(home)?;
        let lock_path = home.join("hosts.json.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        rustix::fs::flock(lock.as_fd(), rustix::fs::FlockOperation::LockExclusive).with_context(|| format!("lock {}", lock_path.display()))?;
        let mut hosts = Self::load(home)?;
        let value = change(&mut hosts)?;
        let path = file(home);
        let tmp = home.join(format!("hosts.json.{}.tmp", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        std::io::Write::write_all(&mut out, &serde_json::to_vec_pretty(&hosts)?)?;
        out.sync_all()?;
        std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))?;
        Ok(value)
    }

    /// A name for a new host with `id` from its `hostname`, numbered when
    /// taken; `None` when the host is already known (it keeps its name).
    pub fn free_name(&self, id: &str, hostname: &str) -> Option<String> {
        if self.hosts.iter().any(|h| h.id == id) {
            return None;
        }
        let base = if hostname.is_empty() { "silo host" } else { hostname };
        (1..).map(|n| if n == 1 { base.to_owned() } else { format!("{base} {n}") }).find(|name| !self.hosts.iter().any(|h| &h.name == name))
    }

    /// Save a paired host: a known EndpointId takes the new route and keeps
    /// its name unless the user `chose` one; a new host is named `chosen`,
    /// else its `hostname` (see [`Hosts::free_name`]).
    ///
    /// # Errors
    /// `chosen` already belongs to another host.
    pub fn pair(&mut self, addr: EndpointAddr, chosen: Option<String>, hostname: &str) -> anyhow::Result<&Host> {
        let name = chosen.or_else(|| self.free_name(&addr.id.to_string(), hostname));
        self.upsert(addr, name)
    }

    /// Add or update the host with `addr`'s id; `name` defaults to the
    /// existing name, else the id's first 10 characters.
    ///
    /// # Errors
    /// `name` already belongs to another host.
    pub fn upsert(&mut self, addr: EndpointAddr, name: Option<String>) -> anyhow::Result<&Host> {
        let id = addr.id.to_string();
        if let Some(name) = &name
            && self.hosts.iter().any(|h| &h.name == name && h.id != id)
        {
            bail!("host name {name:?} is already used");
        }
        let index = match self.hosts.iter().position(|h| h.id == id) {
            Some(index) => {
                let host = &mut self.hosts[index];
                host.addr = addr;
                if let Some(name) = name {
                    host.name = name;
                }
                index
            }
            None => {
                let name = name.unwrap_or_else(|| id[..10].to_owned());
                self.hosts.push(Host { id, name, addr });
                self.hosts.len() - 1
            }
        };
        Ok(&self.hosts[index])
    }

    /// A host by name, id or unique id prefix.
    ///
    /// # Errors
    /// No match, or an ambiguous prefix.
    pub fn find(&self, key: &str) -> anyhow::Result<&Host> {
        if let Some(host) = self.hosts.iter().find(|h| h.name == key || h.id == key) {
            return Ok(host);
        }
        let mut matches = self.hosts.iter().filter(|h| h.id.starts_with(key));
        match (matches.next(), matches.next()) {
            (Some(host), None) => Ok(host),
            (Some(_), Some(_)) => bail!("{key:?} matches several hosts"),
            _ => bail!("no host {key:?}; add one with `silo hosts add <ticket>`"),
        }
    }

    /// The host `key` (see [`Hosts::find`]), or every host when `None`.
    ///
    /// # Errors
    /// See [`Hosts::find`]; no hosts at all.
    pub fn select(&self, key: Option<&str>) -> anyhow::Result<Vec<Host>> {
        match key {
            Some(key) => Ok(vec![self.find(key)?.clone()]),
            None if self.hosts.is_empty() => bail!("no hosts; add one with `silo remote install` or `silo hosts add`"),
            None => Ok(self.hosts.clone()),
        }
    }

    /// Remove a host by name, id or unique id prefix.
    ///
    /// # Errors
    /// See [`Hosts::find`].
    pub fn remove(&mut self, key: &str) -> anyhow::Result<Host> {
        let id = self.find(key)?.id.clone();
        let index = self.hosts.iter().position(|h| h.id == id).expect("found");
        Ok(self.hosts.remove(index))
    }
}

#[cfg(test)]
mod tests {
    use silo_transport::Identity;

    use super::*;

    fn addr(dir: &Path) -> EndpointAddr {
        EndpointAddr::new(Identity::load_or_create(dir).unwrap().id())
    }

    #[test]
    fn hosts_round_trip_and_dedupe() {
        let home = std::env::temp_dir().join(format!("silo-client-hosts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (a, b) = (addr(&home.join("a")), addr(&home.join("b")));
        assert!(Hosts::load(&home).unwrap().hosts.is_empty());
        let moved = a.clone().with_ip_addr(([10, 0, 0, 1], 1).into());
        let hosts = Hosts::update(&home, |hosts| {
            hosts.upsert(a.clone(), Some("work".into())).unwrap();
            hosts.upsert(b.clone(), None).unwrap();
            assert!(hosts.upsert(b.clone(), Some("work".into())).is_err(), "names are unique");
            // Same id again: updated in place, name kept.
            hosts.upsert(moved.clone(), None).unwrap();
            assert_eq!(hosts.hosts.len(), 2);
            Ok(hosts.clone())
        })
        .unwrap();

        let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(home.join("hosts.json")).unwrap().permissions());
        assert_eq!(mode & 0o777, 0o600);
        let loaded = Hosts::load(&home).unwrap();
        assert_eq!(loaded, hosts);
        assert_eq!(loaded.find("work").unwrap().addr, moved);
        assert_eq!(loaded.find(&a.id.to_string()[..6]).unwrap().name, "work");
        assert_eq!(loaded.find(&b.id.to_string()).unwrap().name, b.id.to_string()[..10]);
        assert!(loaded.find("nope").is_err());

        Hosts::update(&home, |hosts| hosts.remove("work").map(drop)).unwrap();
        assert_eq!(Hosts::load(&home).unwrap().hosts.len(), 1);
        // A failed change writes nothing; an unreadable file is never overwritten.
        assert!(Hosts::update(&home, |hosts| hosts.remove("nope").map(drop)).is_err());
        assert_eq!(Hosts::load(&home).unwrap().hosts.len(), 1);
        std::fs::write(home.join("hosts.json"), "{ not json").unwrap();
        assert!(Hosts::update(&home, |_| Ok(())).is_err());
        assert_eq!(std::fs::read_to_string(home.join("hosts.json")).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn a_known_host_keeps_its_name_when_paired_again() {
        let home = std::env::temp_dir().join(format!("silo-client-repair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let (a, b) = (addr(&home.join("a")), addr(&home.join("b")));
        let mut hosts = Hosts::default();
        assert_eq!(hosts.pair(a.clone(), None, "batmini.local").unwrap().name, "batmini.local", "a new host takes its hostname");
        hosts.hosts[0].name = "mini".into();
        // `silo remote install` again: same key, new route, name kept.
        let moved = a.clone().with_ip_addr(([10, 0, 0, 1], 1).into());
        let host = hosts.pair(moved.clone(), None, "batmini.local").unwrap().clone();
        assert_eq!((host.name.as_str(), &host.addr, hosts.hosts.len()), ("mini", &moved, 1));
        // `silo hosts add --name` is the user's choice.
        assert_eq!(hosts.pair(moved, Some("studio".into()), "batmini.local").unwrap().name, "studio");
        // A fresh key is another host, named afresh.
        assert_eq!(hosts.pair(b, None, "studio").unwrap().name, "studio 2");
        let _ = std::fs::remove_dir_all(home);
    }
}
