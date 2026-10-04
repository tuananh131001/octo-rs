//! `Octo.Models.Settings.ServerSettings` (the `Server` section).

use serde::{Deserialize, Serialize};

/// Settings about Octo's own address, as opposed to the upstream servers it talks to.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ServerSettings {
    /// The address Octo is reachable at from outside the local network, for people who
    /// put a reverse proxy or tunnel in front of it (default: empty).
    /// Environment variable: SERVER__PUBLICURL
    ///
    /// Octo cannot work this out on its own. It sees the public hostname on relayed
    /// Subsonic calls, but the admin dashboard is the one place that would display it
    /// and that is exactly the path a sane proxy setup keeps off the public internet,
    /// so the value never arrives where it is needed. It is also not worth detecting:
    /// whoever configured the proxy chose this hostname and already typed it into a
    /// client to test it. This field exists so they only have to remember it once.
    ///
    /// Purely informational. Nothing routes or validates against it; it is displayed
    /// on the dashboard as a second copyable address next to the local one.
    pub public_url: String,
}
