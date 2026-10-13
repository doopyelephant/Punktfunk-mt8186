//! Plugins on this host: the runner and each plugin's access, and the store that installs
//! them from signed catalogs.

pub(crate) mod plugins;
// Running plugins and their UI credentials; the `/plugins` routes sit on it.
pub(crate) mod registry;
// Signed catalogs and install jobs via the `plugins` runner — design/plugin-store.md.
pub(crate) mod store;
