//! The ports `serve` listens on, resolved in one place.
//!
//! `serve` hands [`ListenPlan::resolve`] the flags it parsed and its environment's values.
//! `service install` hands [`ListenPlan::from_host_env`] the `host.env` that becomes both, so
//! the firewall rule names the port the host binds. Each value is flag > env > default, and a
//! blank env value is unset. Neither is a registry row: the console never moves a port.

use std::net::{Ipv4Addr, SocketAddr};

/// The native QUIC port: control, and the video that rides it.
pub const NATIVE_PORT: u16 = 9777;
/// The management API's port, beside the GameStream block (47984…48010).
pub const MGMT_PORT: u16 = 47990;

const MGMT_BIND_ENV: &str = "PUNKTFUNK_MGMT_BIND";
const NATIVE_PORT_ENV: &str = "PUNKTFUNK_NATIVE_PORT";

/// What `serve` binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListenPlan {
    /// The management API. Every interface unless set, so paired clients browse over mTLS.
    pub mgmt: SocketAddr,
    pub native_port: u16,
}

impl Default for ListenPlan {
    fn default() -> Self {
        ListenPlan {
            mgmt: SocketAddr::from((Ipv4Addr::UNSPECIFIED, MGMT_PORT)),
            native_port: NATIVE_PORT,
        }
    }
}

/// An env value that names no address or port. `serve` refuses to start on one.
#[derive(Debug, PartialEq, Eq)]
pub struct BadEnv {
    name: &'static str,
    value: String,
    want: &'static str,
}

impl std::fmt::Display for BadEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let BadEnv { name, value, want } = self;
        write!(f, "bad {name} '{value}' (want {want})")
    }
}

impl std::error::Error for BadEnv {}

/// One value: the flag, else the env value unless blank, else `default`. `Err` is an env
/// value that did not parse.
fn pick<T: std::str::FromStr>(flag: Option<T>, env: Option<&str>, default: T) -> Result<T, &str> {
    match (flag, env.map(str::trim).filter(|s| !s.is_empty())) {
        (Some(v), _) => Ok(v),
        (None, Some(raw)) => raw.parse().map_err(|_| raw),
        (None, None) => Ok(default),
    }
}

impl ListenPlan {
    /// `serve`'s plan: the flags it parsed, then its environment's raw values.
    pub fn resolve(
        mgmt_flag: Option<SocketAddr>,
        native_flag: Option<u16>,
        mgmt_env: Option<&str>,
        native_env: Option<&str>,
    ) -> Result<ListenPlan, BadEnv> {
        let d = ListenPlan::default();
        let bad = |name, want| {
            move |value: &str| BadEnv {
                name,
                value: value.to_string(),
                want,
            }
        };
        Ok(ListenPlan {
            mgmt: pick(mgmt_flag, mgmt_env, d.mgmt).map_err(bad(MGMT_BIND_ENV, "IP:PORT"))?,
            native_port: pick(native_flag, native_env, d.native_port)
                .map_err(bad(NATIVE_PORT_ENV, "a port"))?,
        })
    }

    /// The plan `serve` resolves once the Windows service has loaded this `host.env`: flags
    /// from its `PUNKTFUNK_HOST_CMD`, env from its lines. A value `serve` refuses to start on
    /// keeps the default here, because the rule has to name some port.
    pub fn from_host_env(text: &str) -> ListenPlan {
        let env = |key| pf_paths::env_file::get(text, key);
        let cmd = env("PUNKTFUNK_HOST_CMD").unwrap_or("");
        let flag = |name: &str| {
            let mut words = cmd.split_whitespace();
            words.find(|w| *w == name).and_then(|_| words.next())
        };
        let d = ListenPlan::default();
        let mgmt = flag("--mgmt-bind").map(|v| v.parse().unwrap_or(d.mgmt));
        let native = flag("--native-port").map(|v| v.parse().unwrap_or(d.native_port));
        ListenPlan {
            mgmt: pick(mgmt, env(MGMT_BIND_ENV), d.mgmt).unwrap_or(d.mgmt),
            native_port: pick(native, env(NATIVE_PORT_ENV), d.native_port).unwrap_or(d.native_port),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mgmt_port(host_env: &str) -> u16 {
        ListenPlan::from_host_env(host_env).mgmt.port()
    }

    fn native_port(host_env: &str) -> u16 {
        ListenPlan::from_host_env(host_env).native_port
    }

    /// A host moved off Sunshine's 47990 opens the port it binds; clients learn it in-band.
    #[test]
    fn the_mgmt_rule_follows_the_host_env_bind() {
        assert_eq!(mgmt_port(""), 47990);
        assert_eq!(mgmt_port("PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\r\n"), 47991);
        assert_eq!(mgmt_port("# PUNKTFUNK_MGMT_BIND=0.0.0.0:48123\n"), 47990);
        assert_eq!(
            mgmt_port("PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\nPUNKTFUNK_MGMT_BIND=\n"),
            47990
        );
        assert_eq!(mgmt_port("PUNKTFUNK_MGMT_BIND=\"[::]:48123\"\n"), 48123);
        assert_eq!(mgmt_port("PUNKTFUNK_MGMT_BIND=nonsense\n"), 47990);
        let cmd = "PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\nPUNKTFUNK_HOST_CMD=serve --mgmt-bind 0.0.0.0:48123\n";
        assert_eq!(
            mgmt_port(cmd),
            48123,
            "the command line outranks the env line"
        );
    }

    /// A native port moved in host.env or on the host command is the one the UDP rule opens.
    #[test]
    fn the_native_rule_follows_the_serve_port() {
        assert_eq!(native_port(""), 9777);
        assert_eq!(native_port("PUNKTFUNK_NATIVE_PORT=9800\r\n"), 9800);
        assert_eq!(native_port("# PUNKTFUNK_NATIVE_PORT=9800\n"), 9777);
        assert_eq!(native_port("PUNKTFUNK_NATIVE_PORT=\n"), 9777);
        assert_eq!(
            native_port(
                "PUNKTFUNK_NATIVE_PORT=9800\nPUNKTFUNK_HOST_CMD=\"serve --native-port 9900\"\n"
            ),
            9900
        );
        assert_eq!(
            native_port("PUNKTFUNK_NATIVE_PORT=9800\nPUNKTFUNK_HOST_CMD=serve --gamestream\n"),
            9800
        );
    }

    /// `serve` takes a flag over the env value, the default for a blank one, and refuses a
    /// value that is neither an address nor a port, in the words it always used.
    #[test]
    fn serve_resolves_flag_then_env_then_default() {
        let at = |s: &str| s.parse::<SocketAddr>().unwrap();
        let plan = |mf, nf, me, ne| ListenPlan::resolve(mf, nf, me, ne);
        assert_eq!(plan(None, None, None, None), Ok(ListenPlan::default()));
        assert_eq!(ListenPlan::default().mgmt, at("0.0.0.0:47990"));
        assert_eq!(
            plan(None, None, Some("127.0.0.1:47991"), Some(" 9800 ")),
            Ok(ListenPlan {
                mgmt: at("127.0.0.1:47991"),
                native_port: 9800
            })
        );
        assert_eq!(
            plan(Some(at("[::1]:1")), Some(2), Some("nonsense"), Some("x")),
            Ok(ListenPlan {
                mgmt: at("[::1]:1"),
                native_port: 2
            }),
            "a flag is not second-guessed by the env value under it"
        );
        assert_eq!(
            plan(None, None, Some(" "), Some("")),
            Ok(ListenPlan::default())
        );
        let bad = |me, ne| plan(None, None, me, ne).unwrap_err().to_string();
        assert_eq!(
            bad(Some("47991"), None),
            "bad PUNKTFUNK_MGMT_BIND '47991' (want IP:PORT)"
        );
        assert_eq!(
            bad(None, Some("abc")),
            "bad PUNKTFUNK_NATIVE_PORT 'abc' (want a port)"
        );
    }
}
