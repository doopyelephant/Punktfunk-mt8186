//! JNI seam for the management API over the stream port (`punktfunk_core::client::mgmt`), so a
//! paired device reads the library from a host whose TCP 47990 is closed. Like [`crate::wol`]
//! it takes no session handle and links into the host workspace build.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::{jint, jstring};
use jni::EnvUnowned;
use punktfunk_core::client::mgmt::MgmtTunnel;
use std::time::Duration;

/// `NativeBridge.nativeMgmtGet(host, port, certPem, keyPem, pinHex, path, timeoutMs): String?` —
/// `GET path` over the stream port as this device, refusing any host but `pinHex`. The reply as
/// its status, a newline, then its body; null when the port carried nothing, so HTTPS may.
/// Blocking — Kotlin runs it off the main thread.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeMgmtGet<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    cert_pem: JString<'local>,
    key_pem: JString<'local>,
    pin_hex: JString<'local>,
    path: JString<'local>,
    timeout_ms: jint,
) -> jstring {
    env.with_env(|env| -> jni::errors::Result<jstring> {
        let host: String = host.try_to_string(env)?;
        let cert: String = cert_pem.try_to_string(env)?;
        let key: String = key_pem.try_to_string(env)?;
        let pin_hex: String = pin_hex.try_to_string(env)?;
        let path: String = path.try_to_string(env)?;
        let Some(pin) = punktfunk_core::fp::parse_hex32(&pin_hex) else {
            return Ok(std::ptr::null_mut());
        };
        let port = port.clamp(0, u16::MAX as jint) as u16;
        let wait = Duration::from_millis(timeout_ms.max(0) as u64);
        let reply = MgmtTunnel::open(&host, port, (&cert, &key), Some(pin), wait)
            .and_then(|t| t.request("GET", &path, &[]));
        match reply {
            Ok(r) => {
                let line = format!("{}\n{}", r.status, String::from_utf8_lossy(&r.body));
                Ok(env.new_string(line)?.into_raw())
            }
            Err(e) => {
                log::debug!("mgmt over the stream port: {e}");
                Ok(std::ptr::null_mut())
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}
