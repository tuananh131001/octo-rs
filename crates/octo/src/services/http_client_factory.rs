//! `IHttpClientFactory.CreateClient()`, the default named client `Program.cs` registered with
//! `AddHttpClient()`, and what its failures looked like.
//!
//! The .NET default client: a 100 second `Timeout` covering the whole buffered exchange, no
//! automatic decompression (so no `Accept-Encoding` is sent and a body arrives as the server
//! wrote it), no default request headers, and redirects followed.

use std::error::Error as _;
use std::time::Duration;

/// `HttpClient.Timeout`'s default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(100);

/// The default client, for buffered calls: the timeout covers reading the body too, as
/// `HttpCompletionOption.ResponseContentRead` did.
pub fn default_client() -> reqwest::Client {
    builder()
        .timeout(DEFAULT_TIMEOUT)
        .build()
        .expect("the default HTTP client builds")
}

/// The same client for calls made with `HttpCompletionOption.ResponseHeadersRead`, whose body
/// is then streamed for as long as it lasts: `HttpClient.Timeout` only bounded the wait for the
/// headers, which callers apply themselves with [`DEFAULT_TIMEOUT`].
pub fn streaming_client() -> reqwest::Client {
    builder().build().expect("the streaming HTTP client builds")
}

fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().no_gzip().no_deflate()
}

/// The `TaskCanceledException` message of an `HttpClient` that gave up waiting.
pub fn timeout_message(timeout: Duration) -> String {
    let seconds = timeout.as_secs_f64();
    let shown = if seconds.fract() == 0.0 {
        format!("{}", seconds as u64)
    } else {
        format!("{seconds}")
    };
    format!("The request was canceled due to the configured HttpClient.Timeout of {shown} seconds elapsing.")
}

/// The message .NET's `HttpRequestException` carried for a connection that failed, as far as
/// it can be told from reqwest's error: `Connection refused (host:port)`, `Name or service not
/// known (host:port)` (the resolver's own words, as .NET used getaddrinfo too). Anything else
/// gets reqwest's description.
pub fn connect_failure_message(error: &reqwest::Error) -> String {
    let endpoint = error.url().map(|url| {
        format!(
            "{}:{}",
            url.host_str().unwrap_or(""),
            url.port_or_known_default().unwrap_or(0)
        )
    });
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::ConnectionRefused
        {
            return with_endpoint("Connection refused", endpoint.as_deref());
        }
        let text = cause.to_string();
        if let Some(reason) = text.split("failed to lookup address information: ").nth(1) {
            return with_endpoint(reason.trim(), endpoint.as_deref());
        }
        source = cause.source();
    }
    error.to_string()
}

fn with_endpoint(reason: &str, endpoint: Option<&str>) -> String {
    match endpoint {
        Some(endpoint) => format!("{reason} ({endpoint})"),
        None => reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_messages_read_as_dotnet_wrote_them() {
        assert_eq!(
            timeout_message(DEFAULT_TIMEOUT),
            "The request was canceled due to the configured HttpClient.Timeout of 100 seconds elapsing."
        );
        assert_eq!(
            timeout_message(Duration::from_millis(300)),
            "The request was canceled due to the configured HttpClient.Timeout of 0.3 seconds elapsing."
        );
    }

    #[tokio::test]
    async fn a_refused_connection_names_the_endpoint() {
        // Port 1 on loopback: nothing listens there.
        let error = default_client()
            .get("http://127.0.0.1:1/x")
            .send()
            .await
            .expect_err("nothing listens on port 1");
        assert_eq!(
            connect_failure_message(&error),
            "Connection refused (127.0.0.1:1)"
        );
    }
}
