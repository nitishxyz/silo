//! Paired remote hosts, stored in `<home>/client/hosts.json`, keyed by the
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
    client_dir(home).join("hosts.json")
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

    /// # Errors
    /// The file cannot be written.
    pub fn save(&self, home: &Path) -> anyhow::Result<()> {
        let path = file(home);
        // Shared with the client identity, which requires a private dir.
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700).create(client_dir(home))?;
        let tmp = path.with_extension("json.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::os::unix::fs::OpenOptionsExt::mode(std::fs::OpenOptions::new().write(true).create_new(true), 0o600).open(&tmp)?;
        std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))
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
