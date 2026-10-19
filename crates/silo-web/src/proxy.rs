//! PAC script that sends server-private hosts through a tunnel proxy (PHASE6 §5).

/// PAC routing exactly the server-private hosts to `PROXY 127.0.0.1:<port>`, all else `DIRECT`.
pub fn pac_script(port: u16) -> String {
    format!(
        r#"function FindProxyForURL(url, host) {{
  var h = host.toLowerCase();
  if (h.charAt(0) == "[") h = h.substring(1, h.length - 1);
  var tunnel = "PROXY 127.0.0.1:{port}";
  if (h == "localhost" || dnsDomainIs(h, ".localhost")) return tunnel;
  if (/^\d+\.\d+\.\d+\.\d+$/.test(h)) {{
    if (isInNet(h, "127.0.0.0", "255.0.0.0") || isInNet(h, "10.0.0.0", "255.0.0.0") ||
        isInNet(h, "172.16.0.0", "255.240.0.0") || isInNet(h, "192.168.0.0", "255.255.0.0") ||
        isInNet(h, "169.254.0.0", "255.255.0.0")) return tunnel;
    return "DIRECT";
  }}
  if (h.indexOf(":") >= 0) {{
    if (h == "::1" || /^f[cd][0-9a-f]{{2}}:/.test(h) || /^fe[89ab][0-9a-f]:/.test(h)) return tunnel;
  }}
  return "DIRECT";
}}
"#
    )
}

/// The PAC as a `data:` URL for Chromium's `proxy.pac_url` preference.
pub(crate) fn pac_data_url(port: u16) -> String {
    let script = pac_script(port);
    let mut url = String::from("data:application/x-ns-proxy-autoconfig,");
    for byte in script.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pac_routes_server_private_hosts_only() {
        let pac = pac_script(5555);
        assert!(pac.contains("PROXY 127.0.0.1:5555"));
        for needle in [
            "\"localhost\"",
            "\".localhost\"",
            "127.0.0.0",
            "10.0.0.0",
            "172.16.0.0",
            "192.168.0.0",
            "169.254.0.0",
            "::1",
        ] {
            assert!(pac.contains(needle), "{needle}");
        }
        assert!(pac.contains("return \"DIRECT\";"));
    }

    #[test]
    fn pac_data_url_is_percent_encoded() {
        let url = pac_data_url(1);
        assert!(
            url.starts_with("data:application/x-ns-proxy-autoconfig,function%20FindProxyForURL")
        );
        assert!(!url[url.find(',').unwrap() + 1..].contains([' ', '"', '\n']));
    }
}
