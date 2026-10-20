//! Paired remote hosts, stored in `<home>/hosts.json` (shared with the
//! app, as is the client identity in `<home>/client`), keyed by the
//! host's canonical EndpointId.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use silo_client::transport::EndpointAddr;

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

/// The CLI's client state dir (identity and hosts).
#[must_use]
pub fn client_dir(home: &Path) -> PathBuf {
    home.join("client")
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
