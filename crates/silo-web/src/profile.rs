//! Per-profile request contexts (`<root>/profiles/<name>`), optionally proxied through a tunnel.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::path::Path;

use anyhow::{Context, Result, bail};
use cef::rc::Rc as _;
use cef::*;

use crate::proxy::pac_data_url;

pub const DEFAULT: &str = "default";

/// A browser profile: its own cookies, storage and proxy settings.
#[derive(Clone)]
pub struct Profile {
    pub name: String,
    pub(crate) context: RequestContext,
}

/// Directory name for a profile (`"<server>/<name>"` → `"<server>_<name>"`).
pub(crate) fn dir_name(name: &str) -> Result<String> {
    let dir: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if dir.is_empty() || dir.chars().all(|c| c == '.') {
        bail!("invalid profile name {name:?}");
    }
    Ok(dir)
}

/// Sets Chromium's `proxy` preference: PAC to `127.0.0.1:<port>`, or direct when `None`.
fn apply_proxy(context: &RequestContext, port: Option<u16>) -> Result<()> {
    let dict = dictionary_value_create().context("dictionary_value_create")?;
    let (mode, pac) = match port {
        Some(port) => ("pac_script", Some(pac_data_url(port))),
        None => ("direct", None),
    };
    dict.set_string(Some(&CefString::from("mode")), Some(&CefString::from(mode)));
    if let Some(pac) = pac {
        dict.set_string(
            Some(&CefString::from("pac_url")),
            Some(&CefString::from(pac.as_str())),
        );
    }
    let mut dict = dict;
    let mut value = value_create().context("value_create")?;
    value.set_dictionary(Some(&mut dict));
    let name = CefString::from("proxy");
    // `CefString::default()` marshals as a null pointer and libcef rejects a null `error` out-param.
    let mut error = CefString::from("-");
    if context.set_preference(Some(&name), Some(&mut value), Some(&mut error)) == 0 {
        let (has, can) = (
            context.has_preference(Some(&name)),
            context.can_set_preference(Some(&name)),
        );
        bail!("set proxy preference failed (has={has} can_set={can}): {error}");
    }
    Ok(())
}

wrap_request_context_handler! {
    struct ProxyOnInit {
        port: Option<u16>,
    }

    impl RequestContextHandler {
        fn on_request_context_initialized(&self, context: Option<&mut RequestContext>) {
            let Some(context) = context else { return };
            if let Err(err) = apply_proxy(context, self.port) {
                tracing::error!("silo-web: {err:#}");
            }
        }
    }
}

impl Profile {
    /// The global context (`profiles/default`, `Settings::cache_path`).
    pub(crate) fn global() -> Result<Profile> {
        let context = request_context_get_global_context().context("no global request context")?;
        Ok(Profile {
            name: DEFAULT.to_owned(),
            context,
        })
    }

    /// Creates the context in `profiles/<name>`; the proxy is applied once it initializes.
    pub(crate) fn create(profiles: &Path, name: &str, tunnel_port: Option<u16>) -> Result<Profile> {
        let path = profiles.join(dir_name(name)?);
        let settings = RequestContextSettings {
            cache_path: CefString::from(path.to_string_lossy().as_ref()),
            persist_session_cookies: 1,
            ..Default::default()
        };
        let mut handler = ProxyOnInit::new(tunnel_port);
        let context = request_context_create_context(Some(&settings), Some(&mut handler))
            .context("request_context_create_context")?;
        Ok(Profile {
            name: name.to_owned(),
            context,
        })
    }

    /// Re-points the tunnel proxy (e.g. after the local proxy port changes). UI thread only.
    pub fn set_tunnel_port(&self, port: Option<u16>) -> Result<()> {
        apply_proxy(&self.context, port)
    }
}

#[cfg(test)]
mod tests {
    use super::dir_name;

    #[test]
    fn profile_dirs_are_flat_and_safe() {
        assert_eq!(dir_name("default").unwrap(), "default");
        assert_eq!(dir_name("laptop/work").unwrap(), "laptop_work");
        assert_eq!(dir_name("../x").unwrap(), ".._x");
        assert!(dir_name("..").is_err());
        assert!(dir_name("").is_err());
    }
}
