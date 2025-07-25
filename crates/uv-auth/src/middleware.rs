use std::sync::{Arc, LazyLock};

use anyhow::{anyhow, format_err};
use http::{Extensions, StatusCode};
use netrc::Netrc;
use reqwest::{Request, Response};
use reqwest_middleware::{Error, Middleware, Next};
use tracing::{debug, trace, warn};

use crate::providers::HuggingFaceProvider;
use crate::{
    CREDENTIALS_CACHE, CredentialsCache, KeyringProvider,
    cache::FetchUrl,
    credentials::{Credentials, Username},
    index::{AuthPolicy, Indexes},
    realm::Realm,
};
use uv_redacted::DisplaySafeUrl;

/// Strategy for loading netrc files.
enum NetrcMode {
    Automatic(LazyLock<Option<Netrc>>),
    Enabled(Netrc),
    Disabled,
}

impl Default for NetrcMode {
    fn default() -> Self {
        NetrcMode::Automatic(LazyLock::new(|| match Netrc::new() {
            Ok(netrc) => Some(netrc),
            Err(netrc::Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                debug!("No netrc file found");
                None
            }
            Err(err) => {
                warn!("Error reading netrc file: {err}");
                None
            }
        }))
    }
}

impl NetrcMode {
    /// Get the parsed netrc file if enabled.
    fn get(&self) -> Option<&Netrc> {
        match self {
            NetrcMode::Automatic(lock) => lock.as_ref(),
            NetrcMode::Enabled(netrc) => Some(netrc),
            NetrcMode::Disabled => None,
        }
    }
}

/// A middleware that adds basic authentication to requests.
///
/// Uses a cache to propagate credentials from previously seen requests and
/// fetches credentials from a netrc file and the keyring.
pub struct AuthMiddleware {
    netrc: NetrcMode,
    keyring: Option<KeyringProvider>,
    cache: Option<CredentialsCache>,
    /// Auth policies for specific URLs.
    indexes: Indexes,
    /// Set all endpoints as needing authentication. We never try to send an
    /// unauthenticated request, avoiding cloning an uncloneable request.
    only_authenticated: bool,
}

impl AuthMiddleware {
    pub fn new() -> Self {
        Self {
            netrc: NetrcMode::default(),
            keyring: None,
            cache: None,
            indexes: Indexes::new(),
            only_authenticated: false,
        }
    }

    /// Configure the [`Netrc`] credential file to use.
    ///
    /// `None` disables authentication via netrc.
    #[must_use]
    pub fn with_netrc(mut self, netrc: Option<Netrc>) -> Self {
        self.netrc = if let Some(netrc) = netrc {
            NetrcMode::Enabled(netrc)
        } else {
            NetrcMode::Disabled
        };
        self
    }

    /// Configure the [`KeyringProvider`] to use.
    #[must_use]
    pub fn with_keyring(mut self, keyring: Option<KeyringProvider>) -> Self {
        self.keyring = keyring;
        self
    }

    /// Configure the [`CredentialsCache`] to use.
    #[must_use]
    pub fn with_cache(mut self, cache: CredentialsCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Configure the [`AuthPolicy`]s to use for URLs.
    #[must_use]
    pub fn with_indexes(mut self, indexes: Indexes) -> Self {
        self.indexes = indexes;
        self
    }

    /// Set all endpoints as needing authentication. We never try to send an
    /// unauthenticated request, avoiding cloning an uncloneable request.
    #[must_use]
    pub fn with_only_authenticated(mut self, only_authenticated: bool) -> Self {
        self.only_authenticated = only_authenticated;
        self
    }

    /// Get the configured authentication store.
    ///
    /// If not set, the global store is used.
    fn cache(&self) -> &CredentialsCache {
        self.cache.as_ref().unwrap_or(&CREDENTIALS_CACHE)
    }
}

impl Default for AuthMiddleware {
    fn default() -> Self {
        AuthMiddleware::new()
    }
}

#[async_trait::async_trait]
impl Middleware for AuthMiddleware {
    /// Handle authentication for a request.
    ///
    /// ## If the request has a username and password
    ///
    /// We already have a fully authenticated request and we don't need to perform a look-up.
    ///
    /// - Perform the request
    /// - Add the username and password to the cache if successful
    ///
    /// ## If the request only has a username
    ///
    /// We probably need additional authentication, because a username is provided.
    /// We'll avoid making a request we expect to fail and look for a password.
    /// The discovered credentials must have the requested username to be used.
    ///
    /// - Check the cache (index URL or realm key) for a password
    /// - Check the netrc for a password
    /// - Check the keyring for a password
    /// - Perform the request
    /// - Add the username and password to the cache if successful
    ///
    /// ## If the request has no authentication
    ///
    /// We may or may not need authentication. We'll check for cached credentials for the URL,
    /// which is relatively specific and can save us an expensive failed request. Otherwise,
    /// we'll make the request and look for less-specific credentials on failure i.e. if the
    /// server tells us authorization is needed. This pattern avoids attaching credentials to
    /// requests that do not need them, which can cause some servers to deny the request.
    ///
    /// - Check the cache (URL key)
    /// - Perform the request
    /// - On 401, 403, or 404 check for authentication if there was a cache miss
    ///     - Check the cache (index URL or realm key) for the username and password
    ///     - Check the netrc for a username and password
    ///     - Perform the request again if found
    ///     - Add the username and password to the cache if successful
    async fn handle(
        &self,
        mut request: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        // Check for credentials attached to the request already
        let request_credentials = Credentials::from_request(&request);

        // In the middleware, existing credentials are already moved from the URL
        // to the headers so for display purposes we restore some information
        let url = tracing_url(&request, request_credentials.as_ref());
        let maybe_index_url = self.indexes.index_url_for(request.url());
        let auth_policy = self.indexes.auth_policy_for(request.url());
        trace!("Handling request for {url} with authentication policy {auth_policy}");

        let credentials: Option<Arc<Credentials>> = if matches!(auth_policy, AuthPolicy::Never) {
            None
        } else {
            if let Some(request_credentials) = request_credentials {
                return self
                    .complete_request_with_request_credentials(
                        request_credentials,
                        request,
                        extensions,
                        next,
                        &url,
                        maybe_index_url,
                        auth_policy,
                    )
                    .await;
            }

            // We have no credentials
            trace!("Request for {url} is unauthenticated, checking cache");

            // Check the cache for a URL match first. This can save us from
            // making a failing request
            let credentials = self.cache().get_url(request.url(), &Username::none());
            if let Some(credentials) = credentials.as_ref() {
                request = credentials.authenticate(request);

                // If it's fully authenticated, finish the request
                if credentials.password().is_some() {
                    trace!("Request for {url} is fully authenticated");
                    return self
                        .complete_request(None, request, extensions, next, auth_policy)
                        .await;
                }

                // If we just found a username, we'll make the request then look for password elsewhere
                // if it fails
                trace!("Found username for {url} in cache, attempting request");
            }
            credentials
        };
        let attempt_has_username = credentials
            .as_ref()
            .is_some_and(|credentials| credentials.username().is_some());

        let retry_unauthenticated =
            !self.only_authenticated && !matches!(auth_policy, AuthPolicy::Always);
        let (mut retry_request, response) = if retry_unauthenticated {
            let url = tracing_url(&request, credentials.as_deref());
            if credentials.is_none() {
                trace!("Attempting unauthenticated request for {url}");
            } else {
                trace!("Attempting partially authenticated request for {url}");
            }

            // <https://github.com/TrueLayer/reqwest-middleware/blob/abdf1844c37092d323683c2396b7eefda1418d3c/reqwest-retry/src/middleware.rs#L141-L149>
            // Clone the request so we can retry it on authentication failure
            let retry_request = request.try_clone().ok_or_else(|| {
                Error::Middleware(anyhow!(
                    "Request object is not cloneable. Are you passing a streaming body?"
                        .to_string()
                ))
            })?;

            let response = next.clone().run(request, extensions).await?;

            // If we don't fail with authorization related codes or
            // authentication policy is Never, return the response.
            if !matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED
            ) || matches!(auth_policy, AuthPolicy::Never)
            {
                return Ok(response);
            }

            // Otherwise, search for credentials
            trace!(
                "Request for {url} failed with {}, checking for credentials",
                response.status()
            );

            (retry_request, Some(response))
        } else {
            // For endpoints where we require the user to provide credentials, we don't try the
            // unauthenticated request first.
            trace!("Checking for credentials for {url}");
            (request, None)
        };
        let retry_request_url = DisplaySafeUrl::ref_cast(retry_request.url());

        let username = credentials
            .as_ref()
            .map(|credentials| credentials.to_username())
            .unwrap_or(Username::none());
        let credentials = if let Some(index_url) = maybe_index_url {
            self.cache().get_url(index_url, &username).or_else(|| {
                self.cache()
                    .get_realm(Realm::from(&**retry_request_url), username)
            })
        } else {
            // Since there is no known index for this URL, check if there are credentials in
            // the realm-level cache.
            self.cache()
                .get_realm(Realm::from(&**retry_request_url), username)
        }
        .or(credentials);

        if let Some(credentials) = credentials.as_ref() {
            if credentials.password().is_some() {
                trace!("Retrying request for {url} with credentials from cache {credentials:?}");
                retry_request = credentials.authenticate(retry_request);
                return self
                    .complete_request(None, retry_request, extensions, next, auth_policy)
                    .await;
            }
        }

        // Then, fetch from external services.
        // Here, we use the username from the cache if present.
        if let Some(credentials) = self
            .fetch_credentials(
                credentials.as_deref(),
                retry_request_url,
                maybe_index_url,
                auth_policy,
            )
            .await
        {
            retry_request = credentials.authenticate(retry_request);
            trace!("Retrying request for {url} with {credentials:?}");
            return self
                .complete_request(
                    Some(credentials),
                    retry_request,
                    extensions,
                    next,
                    auth_policy,
                )
                .await;
        }

        if let Some(credentials) = credentials.as_ref() {
            if !attempt_has_username {
                trace!("Retrying request for {url} with username from cache {credentials:?}");
                retry_request = credentials.authenticate(retry_request);
                return self
                    .complete_request(None, retry_request, extensions, next, auth_policy)
                    .await;
            }
        }

        if let Some(response) = response {
            Ok(response)
        } else {
            Err(Error::Middleware(format_err!(
                "Missing credentials for {url}"
            )))
        }
    }
}

impl AuthMiddleware {
    /// Run a request to completion.
    ///
    /// If credentials are present, insert them into the cache on success.
    async fn complete_request(
        &self,
        credentials: Option<Arc<Credentials>>,
        request: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
        auth_policy: AuthPolicy,
    ) -> reqwest_middleware::Result<Response> {
        let Some(credentials) = credentials else {
            // Nothing to insert into the cache if we don't have credentials
            return next.run(request, extensions).await;
        };
        let url = DisplaySafeUrl::from(request.url().clone());
        if matches!(auth_policy, AuthPolicy::Always) && credentials.password().is_none() {
            return Err(Error::Middleware(format_err!("Missing password for {url}")));
        }
        let result = next.run(request, extensions).await;

        // Update the cache with new credentials on a successful request
        if result
            .as_ref()
            .is_ok_and(|response| response.error_for_status_ref().is_ok())
        {
            trace!("Updating cached credentials for {url} to {credentials:?}");
            self.cache().insert(&url, credentials);
        }

        result
    }

    /// Use known request credentials to complete the request.
    async fn complete_request_with_request_credentials(
        &self,
        credentials: Credentials,
        mut request: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
        url: &DisplaySafeUrl,
        index_url: Option<&DisplaySafeUrl>,
        auth_policy: AuthPolicy,
    ) -> reqwest_middleware::Result<Response> {
        let credentials = Arc::new(credentials);

        // If there's a password, send the request and cache
        if credentials.password().is_some() {
            trace!("Request for {url} already contains username and password");
            return self
                .complete_request(Some(credentials), request, extensions, next, auth_policy)
                .await;
        }

        trace!("Request for {url} is missing a password, looking for credentials");

        // There's just a username, try to find a password.
        // If we have an index URL, check the cache for that URL. Otherwise,
        // check for the realm.
        let maybe_cached_credentials = if let Some(index_url) = index_url {
            self.cache()
                .get_url(index_url, credentials.as_username().as_ref())
        } else {
            self.cache()
                .get_realm(Realm::from(request.url()), credentials.to_username())
        };
        if let Some(credentials) = maybe_cached_credentials {
            request = credentials.authenticate(request);
            // Do not insert already-cached credentials
            let credentials = None;
            return self
                .complete_request(credentials, request, extensions, next, auth_policy)
                .await;
        }

        let credentials = if let Some(credentials) = self
            .cache()
            .get_url(request.url(), credentials.as_username().as_ref())
        {
            request = credentials.authenticate(request);
            // Do not insert already-cached credentials
            None
        } else if let Some(credentials) = self
            .fetch_credentials(
                Some(&credentials),
                DisplaySafeUrl::ref_cast(request.url()),
                index_url,
                auth_policy,
            )
            .await
        {
            request = credentials.authenticate(request);
            Some(credentials)
        } else if index_url.is_some() {
            // If this is a known index, we fall back to checking for the realm.
            if let Some(credentials) = self
                .cache()
                .get_realm(Realm::from(request.url()), credentials.to_username())
            {
                request = credentials.authenticate(request);
                Some(credentials)
            } else {
                Some(credentials)
            }
        } else {
            // If we don't find a password, we'll still attempt the request with the existing credentials
            Some(credentials)
        };

        self.complete_request(credentials, request, extensions, next, auth_policy)
            .await
    }

    /// Fetch credentials for a URL.
    ///
    /// Supports netrc file and keyring lookups.
    async fn fetch_credentials(
        &self,
        credentials: Option<&Credentials>,
        url: &DisplaySafeUrl,
        maybe_index_url: Option<&DisplaySafeUrl>,
        auth_policy: AuthPolicy,
    ) -> Option<Arc<Credentials>> {
        let username = Username::from(
            credentials.map(|credentials| credentials.username().unwrap_or_default().to_string()),
        );

        // Fetches can be expensive, so we will only run them _once_ per realm or index URL and username combination
        // All other requests for the same realm or index URL will wait until the first one completes
        let key = if let Some(index_url) = maybe_index_url {
            (FetchUrl::Index(index_url.clone()), username)
        } else {
            (FetchUrl::Realm(Realm::from(&**url)), username)
        };
        if !self.cache().fetches.register(key.clone()) {
            let credentials = self
                .cache()
                .fetches
                .wait(&key)
                .await
                .expect("The key must exist after register is called");

            if credentials.is_some() {
                trace!("Using credentials from previous fetch for {}", key.0);
            } else {
                trace!(
                    "Skipping fetch of credentials for {}, previous attempt failed",
                    key.0
                );
            }

            return credentials;
        }

        // Support for known providers, like Hugging Face.
        if let Some(credentials) = HuggingFaceProvider::credentials_for(url).map(Arc::new) {
            debug!("Found Hugging Face credentials for {url}");
            self.cache().fetches.done(key, Some(credentials.clone()));
            return Some(credentials);
        }

        // Netrc support based on: <https://github.com/gribouille/netrc>.
        let credentials = if let Some(credentials) = self.netrc.get().and_then(|netrc| {
            debug!("Checking netrc for credentials for {url}");
            Credentials::from_netrc(
                netrc,
                url,
                credentials
                    .as_ref()
                    .and_then(|credentials| credentials.username()),
            )
        }) {
            debug!("Found credentials in netrc file for {url}");
            Some(credentials)

        // N.B. The keyring provider performs lookups for the exact URL then falls back to the host.
        //      But, in the absence of an index URL, we cache the result per realm. So in that case,
        //      if a keyring implementation returns different credentials for different URLs in the
        //      same realm we will use the wrong credentials.
        } else if let Some(credentials) = match self.keyring {
            Some(ref keyring) => {
                // The subprocess keyring provider is _slow_ so we do not perform fetches for all
                // URLs; instead, we fetch if there's a username or if the user has requested to
                // always authenticate.
                if let Some(username) = credentials.and_then(|credentials| credentials.username()) {
                    if let Some(index_url) = maybe_index_url {
                        debug!("Checking keyring for credentials for index URL {}@{}", username, index_url);
                        keyring.fetch(DisplaySafeUrl::ref_cast(index_url), Some(username)).await
                    } else {
                        debug!("Checking keyring for credentials for full URL {}@{}", username, url);
                        keyring.fetch(url, Some(username)).await
                    }
                } else if matches!(auth_policy, AuthPolicy::Always) {
                    if let Some(index_url) = maybe_index_url {
                        debug!(
                            "Checking keyring for credentials for index URL {index_url} without username due to `authenticate = always`"
                        );
                        keyring.fetch(DisplaySafeUrl::ref_cast(index_url), None).await
                    } else {
                        None
                    }
                } else {
                    debug!("Skipping keyring fetch for {url} without username; use `authenticate = always` to force");
                    None
                }
            }
            None => None,
        } {
            debug!("Found credentials in keyring for {url}");
            Some(credentials)
        } else {
            None
        }
        .map(Arc::new);

        // Register the fetch for this key
        self.cache().fetches.done(key, credentials.clone());

        credentials
    }
}

fn tracing_url(request: &Request, credentials: Option<&Credentials>) -> DisplaySafeUrl {
    let mut url = DisplaySafeUrl::from(request.url().clone());
    if let Some(creds) = credentials {
        if let Some(username) = creds.username() {
            let _ = url.set_username(username);
        }
        if let Some(password) = creds.password() {
            let _ = url.set_password(Some(password));
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use http::Method;
    use reqwest::Client;
    use tempfile::NamedTempFile;
    use test_log::test;

    use url::Url;
    use wiremock::matchers::{basic_auth, method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::Index;
    use crate::credentials::Password;

    use super::*;

    type Error = Box<dyn std::error::Error>;

    async fn start_test_server(username: &'static str, password: &'static str) -> MockServer {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(basic_auth(username, password))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        server
    }

    fn test_client_builder() -> reqwest_middleware::ClientBuilder {
        reqwest_middleware::ClientBuilder::new(
            Client::builder()
                .build()
                .expect("Reqwest client should build"),
        )
    }

    #[test(tokio::test)]
    async fn test_no_credentials() -> Result<(), Error> {
        let server = start_test_server("user", "password").await;
        let client = test_client_builder()
            .with(AuthMiddleware::new().with_cache(CredentialsCache::new()))
            .build();

        assert_eq!(
            client
                .get(format!("{}/foo", server.uri()))
                .send()
                .await?
                .status(),
            401
        );

        assert_eq!(
            client
                .get(format!("{}/bar", server.uri()))
                .send()
                .await?
                .status(),
            401
        );

        Ok(())
    }

    /// Without seeding the cache, authenticated requests are not cached
    #[test(tokio::test)]
    async fn test_credentials_in_url_no_seed() -> Result<(), Error> {
        let username = "user";
        let password = "password";

        let server = start_test_server(username, password).await;
        let client = test_client_builder()
            .with(AuthMiddleware::new().with_cache(CredentialsCache::new()))
            .build();

        let base_url = Url::parse(&server.uri())?;

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some(password)).unwrap();
        assert_eq!(client.get(url).send().await?.status(), 200);

        // Works for a URL without credentials now
        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Subsequent requests should not require credentials"
        );

        assert_eq!(
            client
                .get(format!("{}/foo", server.uri()))
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths in the same realm"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some("invalid")).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "Credentials in the URL should take precedence and fail"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_credentials_in_url_seed() -> Result<(), Error> {
        let username = "user";
        let password = "password";

        let server = start_test_server(username, password).await;
        let base_url = Url::parse(&server.uri())?;
        let cache = CredentialsCache::new();
        cache.insert(
            &base_url,
            Arc::new(Credentials::basic(
                Some(username.to_string()),
                Some(password.to_string()),
            )),
        );

        let client = test_client_builder()
            .with(AuthMiddleware::new().with_cache(cache))
            .build();

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some(password)).unwrap();
        assert_eq!(client.get(url).send().await?.status(), 200);

        // Works for a URL without credentials too
        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Requests should not require credentials"
        );

        assert_eq!(
            client
                .get(format!("{}/foo", server.uri()))
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths in the same realm"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some("invalid")).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "Credentials in the URL should take precedence and fail"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_credentials_in_url_username_only() -> Result<(), Error> {
        let username = "user";
        let password = "";

        let server = start_test_server(username, password).await;
        let base_url = Url::parse(&server.uri())?;
        let cache = CredentialsCache::new();
        cache.insert(
            &base_url,
            Arc::new(Credentials::basic(Some(username.to_string()), None)),
        );

        let client = test_client_builder()
            .with(AuthMiddleware::new().with_cache(cache))
            .build();

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(None).unwrap();
        assert_eq!(client.get(url).send().await?.status(), 200);

        // Works for a URL without credentials too
        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Requests should not require credentials"
        );

        assert_eq!(
            client
                .get(format!("{}/foo", server.uri()))
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths in the same realm"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some("invalid")).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "Credentials in the URL should take precedence and fail"
        );

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Subsequent requests should not use the invalid credentials"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_netrc_file_default_host() -> Result<(), Error> {
        let username = "user";
        let password = "password";

        let mut netrc_file = NamedTempFile::new()?;
        writeln!(netrc_file, "default login {username} password {password}")?;

        let server = start_test_server(username, password).await;
        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_netrc(Netrc::from_file(netrc_file.path()).ok()),
            )
            .build();

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Credentials should be pulled from the netrc file"
        );

        let mut url = Url::parse(&server.uri())?;
        url.set_username(username).unwrap();
        url.set_password(Some("invalid")).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "Credentials in the URL should take precedence and fail"
        );

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Subsequent requests should not use the invalid credentials"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_netrc_file_matching_host() -> Result<(), Error> {
        let username = "user";
        let password = "password";
        let server = start_test_server(username, password).await;
        let base_url = Url::parse(&server.uri())?;

        let mut netrc_file = NamedTempFile::new()?;
        writeln!(
            netrc_file,
            r"machine {} login {username} password {password}",
            base_url.host_str().unwrap()
        )?;

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_netrc(Some(
                        Netrc::from_file(netrc_file.path()).expect("Test has valid netrc file"),
                    )),
            )
            .build();

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Credentials should be pulled from the netrc file"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some("invalid")).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "Credentials in the URL should take precedence and fail"
        );

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Subsequent requests should not use the invalid credentials"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_netrc_file_mismatched_host() -> Result<(), Error> {
        let username = "user";
        let password = "password";
        let server = start_test_server(username, password).await;

        let mut netrc_file = NamedTempFile::new()?;
        writeln!(
            netrc_file,
            r"machine example.com login {username} password {password}",
        )?;

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_netrc(Some(
                        Netrc::from_file(netrc_file.path()).expect("Test has valid netrc file"),
                    )),
            )
            .build();

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            401,
            "Credentials should not be pulled from the netrc file due to host mismatch"
        );

        let mut url = Url::parse(&server.uri())?;
        url.set_username(username).unwrap();
        url.set_password(Some(password)).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            200,
            "Credentials in the URL should still work"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_netrc_file_mismatched_username() -> Result<(), Error> {
        let username = "user";
        let password = "password";
        let server = start_test_server(username, password).await;
        let base_url = Url::parse(&server.uri())?;

        let mut netrc_file = NamedTempFile::new()?;
        writeln!(
            netrc_file,
            r"machine {} login {username} password {password}",
            base_url.host_str().unwrap()
        )?;

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_netrc(Some(
                        Netrc::from_file(netrc_file.path()).expect("Test has valid netrc file"),
                    )),
            )
            .build();

        let mut url = base_url.clone();
        url.set_username("other-user").unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "The netrc password should not be used due to a username mismatch"
        );

        let mut url = base_url.clone();
        url.set_username("user").unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            200,
            "The netrc password should be used for a matching user"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_keyring() -> Result<(), Error> {
        let username = "user";
        let password = "password";
        let server = start_test_server(username, password).await;
        let base_url = Url::parse(&server.uri())?;

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_keyring(Some(KeyringProvider::dummy([(
                        format!(
                            "{}:{}",
                            base_url.host_str().unwrap(),
                            base_url.port().unwrap()
                        ),
                        username,
                        password,
                    )]))),
            )
            .build();

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            401,
            "Credentials are not pulled from the keyring without a username"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            200,
            "Credentials for the username should be pulled from the keyring"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some("invalid")).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "Password in the URL should take precedence and fail"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        assert_eq!(
            client.get(url.clone()).send().await?.status(),
            200,
            "Subsequent requests should not use the invalid password"
        );

        let mut url = base_url.clone();
        url.set_username("other_user").unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "Credentials are not pulled from the keyring when given another username"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_keyring_always_authenticate() -> Result<(), Error> {
        let username = "user";
        let password = "password";
        let server = start_test_server(username, password).await;
        let base_url = Url::parse(&server.uri())?;

        let indexes = indexes_for(&base_url, AuthPolicy::Always);
        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_keyring(Some(KeyringProvider::dummy([(
                        format!(
                            "{}:{}",
                            base_url.host_str().unwrap(),
                            base_url.port().unwrap()
                        ),
                        username,
                        password,
                    )])))
                    .with_indexes(indexes),
            )
            .build();

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Credentials (including a username) should be pulled from the keyring"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            200,
            "The password for the username should be pulled from the keyring"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some("invalid")).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "Password in the URL should take precedence and fail"
        );

        let mut url = base_url.clone();
        url.set_username("other_user").unwrap();
        assert!(
            matches!(
                client.get(url).send().await,
                Err(reqwest_middleware::Error::Middleware(_))
            ),
            "If the username does not match, a password should not be fetched, and the middleware should fail eagerly since `authenticate = always` is not satisfied"
        );

        Ok(())
    }

    /// We include ports in keyring requests, e.g., `localhost:8000` should be distinct from `localhost`,
    /// unless the server is running on a default port, e.g., `localhost:80` is equivalent to `localhost`.
    /// We don't unit test the latter case because it's possible to collide with a server a developer is
    /// actually running.
    #[test(tokio::test)]
    async fn test_keyring_includes_non_standard_port() -> Result<(), Error> {
        let username = "user";
        let password = "password";
        let server = start_test_server(username, password).await;
        let base_url = Url::parse(&server.uri())?;

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_keyring(Some(KeyringProvider::dummy([(
                        // Omit the port from the keyring entry
                        base_url.host_str().unwrap(),
                        username,
                        password,
                    )]))),
            )
            .build();

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "We should fail because the port is not present in the keyring entry"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_credentials_in_keyring_seed() -> Result<(), Error> {
        let username = "user";
        let password = "password";

        let server = start_test_server(username, password).await;
        let base_url = Url::parse(&server.uri())?;
        let cache = CredentialsCache::new();

        // Seed _just_ the username. We should pull the username from the cache if not present on the
        // URL.
        cache.insert(
            &base_url,
            Arc::new(Credentials::basic(Some(username.to_string()), None)),
        );
        let client = test_client_builder()
            .with(AuthMiddleware::new().with_cache(cache).with_keyring(Some(
                KeyringProvider::dummy([(
                    format!(
                        "{}:{}",
                        base_url.host_str().unwrap(),
                        base_url.port().unwrap()
                    ),
                    username,
                    password,
                )]),
            )))
            .build();

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "The username is pulled from the cache, and the password from the keyring"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            200,
            "Credentials for the username should be pulled from the keyring"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_credentials_in_url_multiple_realms() -> Result<(), Error> {
        let username_1 = "user1";
        let password_1 = "password1";
        let server_1 = start_test_server(username_1, password_1).await;
        let base_url_1 = Url::parse(&server_1.uri())?;

        let username_2 = "user2";
        let password_2 = "password2";
        let server_2 = start_test_server(username_2, password_2).await;
        let base_url_2 = Url::parse(&server_2.uri())?;

        let cache = CredentialsCache::new();
        // Seed the cache with our credentials
        cache.insert(
            &base_url_1,
            Arc::new(Credentials::basic(
                Some(username_1.to_string()),
                Some(password_1.to_string()),
            )),
        );
        cache.insert(
            &base_url_2,
            Arc::new(Credentials::basic(
                Some(username_2.to_string()),
                Some(password_2.to_string()),
            )),
        );

        let client = test_client_builder()
            .with(AuthMiddleware::new().with_cache(cache))
            .build();

        // Both servers should work
        assert_eq!(
            client.get(server_1.uri()).send().await?.status(),
            200,
            "Requests should not require credentials"
        );
        assert_eq!(
            client.get(server_2.uri()).send().await?.status(),
            200,
            "Requests should not require credentials"
        );

        assert_eq!(
            client
                .get(format!("{}/foo", server_1.uri()))
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths in the same realm"
        );
        assert_eq!(
            client
                .get(format!("{}/foo", server_2.uri()))
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths in the same realm"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_credentials_from_keyring_multiple_realms() -> Result<(), Error> {
        let username_1 = "user1";
        let password_1 = "password1";
        let server_1 = start_test_server(username_1, password_1).await;
        let base_url_1 = Url::parse(&server_1.uri())?;

        let username_2 = "user2";
        let password_2 = "password2";
        let server_2 = start_test_server(username_2, password_2).await;
        let base_url_2 = Url::parse(&server_2.uri())?;

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_keyring(Some(KeyringProvider::dummy([
                        (
                            format!(
                                "{}:{}",
                                base_url_1.host_str().unwrap(),
                                base_url_1.port().unwrap()
                            ),
                            username_1,
                            password_1,
                        ),
                        (
                            format!(
                                "{}:{}",
                                base_url_2.host_str().unwrap(),
                                base_url_2.port().unwrap()
                            ),
                            username_2,
                            password_2,
                        ),
                    ]))),
            )
            .build();

        // Both servers do not work without a username
        assert_eq!(
            client.get(server_1.uri()).send().await?.status(),
            401,
            "Requests should require a username"
        );
        assert_eq!(
            client.get(server_2.uri()).send().await?.status(),
            401,
            "Requests should require a username"
        );

        let mut url_1 = base_url_1.clone();
        url_1.set_username(username_1).unwrap();
        assert_eq!(
            client.get(url_1.clone()).send().await?.status(),
            200,
            "Requests with a username should succeed"
        );
        assert_eq!(
            client.get(server_2.uri()).send().await?.status(),
            401,
            "Credentials should not be re-used for the second server"
        );

        let mut url_2 = base_url_2.clone();
        url_2.set_username(username_2).unwrap();
        assert_eq!(
            client.get(url_2.clone()).send().await?.status(),
            200,
            "Requests with a username should succeed"
        );

        assert_eq!(
            client.get(format!("{url_1}/foo")).send().await?.status(),
            200,
            "Requests can be to different paths in the same realm"
        );
        assert_eq!(
            client.get(format!("{url_2}/foo")).send().await?.status(),
            200,
            "Requests can be to different paths in the same realm"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_credentials_in_url_mixed_authentication_in_realm() -> Result<(), Error> {
        let username_1 = "user1";
        let password_1 = "password1";
        let username_2 = "user2";
        let password_2 = "password2";

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path_regex("/prefix_1.*"))
            .and(basic_auth(username_1, password_1))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/prefix_2.*"))
            .and(basic_auth(username_2, password_2))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // Create a third, public prefix
        // It will throw a 401 if it receives credentials
        Mock::given(method("GET"))
            .and(path_regex("/prefix_3.*"))
            .and(basic_auth(username_1, password_1))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/prefix_3.*"))
            .and(basic_auth(username_2, password_2))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/prefix_3.*"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let base_url = Url::parse(&server.uri())?;
        let base_url_1 = base_url.join("prefix_1")?;
        let base_url_2 = base_url.join("prefix_2")?;
        let base_url_3 = base_url.join("prefix_3")?;

        let cache = CredentialsCache::new();

        // Seed the cache with our credentials
        cache.insert(
            &base_url_1,
            Arc::new(Credentials::basic(
                Some(username_1.to_string()),
                Some(password_1.to_string()),
            )),
        );
        cache.insert(
            &base_url_2,
            Arc::new(Credentials::basic(
                Some(username_2.to_string()),
                Some(password_2.to_string()),
            )),
        );

        let client = test_client_builder()
            .with(AuthMiddleware::new().with_cache(cache))
            .build();

        // Both servers should work
        assert_eq!(
            client.get(base_url_1.clone()).send().await?.status(),
            200,
            "Requests should not require credentials"
        );
        assert_eq!(
            client.get(base_url_2.clone()).send().await?.status(),
            200,
            "Requests should not require credentials"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_1/foo")?)
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths in the same realm"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_2/foo")?)
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths in the same realm"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_1_foo")?)
                .send()
                .await?
                .status(),
            401,
            "Requests to paths with a matching prefix but different resource segments should fail"
        );

        assert_eq!(
            client.get(base_url_3.clone()).send().await?.status(),
            200,
            "Requests to the 'public' prefix should not use credentials"
        );

        Ok(())
    }

    #[test(tokio::test)]
    async fn test_credentials_from_keyring_mixed_authentication_in_realm() -> Result<(), Error> {
        let username_1 = "user1";
        let password_1 = "password1";
        let username_2 = "user2";
        let password_2 = "password2";

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path_regex("/prefix_1.*"))
            .and(basic_auth(username_1, password_1))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/prefix_2.*"))
            .and(basic_auth(username_2, password_2))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        // Create a third, public prefix
        // It will throw a 401 if it receives credentials
        Mock::given(method("GET"))
            .and(path_regex("/prefix_3.*"))
            .and(basic_auth(username_1, password_1))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/prefix_3.*"))
            .and(basic_auth(username_2, password_2))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/prefix_3.*"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let base_url = Url::parse(&server.uri())?;
        let base_url_1 = base_url.join("prefix_1")?;
        let base_url_2 = base_url.join("prefix_2")?;
        let base_url_3 = base_url.join("prefix_3")?;

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_keyring(Some(KeyringProvider::dummy([
                        (
                            format!(
                                "{}:{}",
                                base_url_1.host_str().unwrap(),
                                base_url_1.port().unwrap()
                            ),
                            username_1,
                            password_1,
                        ),
                        (
                            format!(
                                "{}:{}",
                                base_url_2.host_str().unwrap(),
                                base_url_2.port().unwrap()
                            ),
                            username_2,
                            password_2,
                        ),
                    ]))),
            )
            .build();

        // Both servers do not work without a username
        assert_eq!(
            client.get(base_url_1.clone()).send().await?.status(),
            401,
            "Requests should require a username"
        );
        assert_eq!(
            client.get(base_url_2.clone()).send().await?.status(),
            401,
            "Requests should require a username"
        );

        let mut url_1 = base_url_1.clone();
        url_1.set_username(username_1).unwrap();
        assert_eq!(
            client.get(url_1.clone()).send().await?.status(),
            200,
            "Requests with a username should succeed"
        );
        assert_eq!(
            client.get(base_url_2.clone()).send().await?.status(),
            401,
            "Credentials should not be re-used for the second prefix"
        );

        let mut url_2 = base_url_2.clone();
        url_2.set_username(username_2).unwrap();
        assert_eq!(
            client.get(url_2.clone()).send().await?.status(),
            200,
            "Requests with a username should succeed"
        );

        assert_eq!(
            client
                .get(base_url.join("prefix_1/foo")?)
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths in the same prefix"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_2/foo")?)
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths in the same prefix"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_1_foo")?)
                .send()
                .await?
                .status(),
            401,
            "Requests to paths with a matching prefix but different resource segments should fail"
        );
        assert_eq!(
            client.get(base_url_3.clone()).send().await?.status(),
            200,
            "Requests to the 'public' prefix should not use credentials"
        );

        Ok(())
    }

    /// Demonstrates "incorrect" behavior in our cache which avoids an expensive fetch of
    /// credentials for _every_ request URL at the cost of inconsistent behavior when
    /// credentials are not scoped to a realm.
    #[test(tokio::test)]
    async fn test_credentials_from_keyring_mixed_authentication_in_realm_same_username()
    -> Result<(), Error> {
        let username = "user";
        let password_1 = "password1";
        let password_2 = "password2";

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path_regex("/prefix_1.*"))
            .and(basic_auth(username, password_1))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/prefix_2.*"))
            .and(basic_auth(username, password_2))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let base_url = Url::parse(&server.uri())?;
        let base_url_1 = base_url.join("prefix_1")?;
        let base_url_2 = base_url.join("prefix_2")?;

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_keyring(Some(KeyringProvider::dummy([
                        (base_url_1.clone(), username, password_1),
                        (base_url_2.clone(), username, password_2),
                    ]))),
            )
            .build();

        // Both servers do not work without a username
        assert_eq!(
            client.get(base_url_1.clone()).send().await?.status(),
            401,
            "Requests should require a username"
        );
        assert_eq!(
            client.get(base_url_2.clone()).send().await?.status(),
            401,
            "Requests should require a username"
        );

        let mut url_1 = base_url_1.clone();
        url_1.set_username(username).unwrap();
        assert_eq!(
            client.get(url_1.clone()).send().await?.status(),
            200,
            "The first request with a username will succeed"
        );
        assert_eq!(
            client.get(base_url_2.clone()).send().await?.status(),
            401,
            "Credentials should not be re-used for the second prefix"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_1/foo")?)
                .send()
                .await?
                .status(),
            200,
            "Subsequent requests can be to different paths in the same prefix"
        );

        let mut url_2 = base_url_2.clone();
        url_2.set_username(username).unwrap();
        assert_eq!(
            client.get(url_2.clone()).send().await?.status(),
            401, // INCORRECT BEHAVIOR
            "A request with the same username and realm for a URL that needs a different password will fail"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_2/foo")?)
                .send()
                .await?
                .status(),
            401, // INCORRECT BEHAVIOR
            "Requests to other paths in the failing prefix will also fail"
        );

        Ok(())
    }

    /// Demonstrates that when an index URL is provided, we avoid "incorrect" behavior
    /// where multiple URLs with the same username and realm share the same realm-level
    /// credentials cache entry.
    #[test(tokio::test)]
    async fn test_credentials_from_keyring_mixed_authentication_different_indexes_same_realm()
    -> Result<(), Error> {
        let username = "user";
        let password_1 = "password1";
        let password_2 = "password2";

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path_regex("/prefix_1.*"))
            .and(basic_auth(username, password_1))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/prefix_2.*"))
            .and(basic_auth(username, password_2))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let base_url = Url::parse(&server.uri())?;
        let base_url_1 = base_url.join("prefix_1")?;
        let base_url_2 = base_url.join("prefix_2")?;
        let indexes = Indexes::from_indexes(vec![
            Index {
                url: DisplaySafeUrl::from(base_url_1.clone()),
                root_url: DisplaySafeUrl::from(base_url_1.clone()),
                auth_policy: AuthPolicy::Auto,
            },
            Index {
                url: DisplaySafeUrl::from(base_url_2.clone()),
                root_url: DisplaySafeUrl::from(base_url_2.clone()),
                auth_policy: AuthPolicy::Auto,
            },
        ]);

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_keyring(Some(KeyringProvider::dummy([
                        (base_url_1.clone(), username, password_1),
                        (base_url_2.clone(), username, password_2),
                    ])))
                    .with_indexes(indexes),
            )
            .build();

        // Both servers do not work without a username
        assert_eq!(
            client.get(base_url_1.clone()).send().await?.status(),
            401,
            "Requests should require a username"
        );
        assert_eq!(
            client.get(base_url_2.clone()).send().await?.status(),
            401,
            "Requests should require a username"
        );

        let mut url_1 = base_url_1.clone();
        url_1.set_username(username).unwrap();
        assert_eq!(
            client.get(url_1.clone()).send().await?.status(),
            200,
            "The first request with a username will succeed"
        );
        assert_eq!(
            client.get(base_url_2.clone()).send().await?.status(),
            401,
            "Credentials should not be re-used for the second prefix"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_1/foo")?)
                .send()
                .await?
                .status(),
            200,
            "Subsequent requests can be to different paths in the same prefix"
        );

        let mut url_2 = base_url_2.clone();
        url_2.set_username(username).unwrap();
        assert_eq!(
            client.get(url_2.clone()).send().await?.status(),
            200,
            "A request with the same username and realm for a URL will use index-specific password"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_2/foo")?)
                .send()
                .await?
                .status(),
            200,
            "Requests to other paths with that prefix will also succeed"
        );

        Ok(())
    }

    /// Demonstrates that when an index' credentials are cached for its realm, we
    /// find those credentials if they're not present in the keyring.
    #[test(tokio::test)]
    async fn test_credentials_from_keyring_shared_authentication_different_indexes_same_realm()
    -> Result<(), Error> {
        let username = "user";
        let password = "password";

        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(basic_auth(username, password))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/prefix_1.*"))
            .and(basic_auth(username, password))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let base_url = Url::parse(&server.uri())?;
        let index_url = base_url.join("prefix_1")?;
        let indexes = Indexes::from_indexes(vec![Index {
            url: DisplaySafeUrl::from(index_url.clone()),
            root_url: DisplaySafeUrl::from(index_url.clone()),
            auth_policy: AuthPolicy::Auto,
        }]);

        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_keyring(Some(KeyringProvider::dummy([(
                        base_url.clone(),
                        username,
                        password,
                    )])))
                    .with_indexes(indexes),
            )
            .build();

        // Index server does not work without a username
        assert_eq!(
            client.get(index_url.clone()).send().await?.status(),
            401,
            "Requests should require a username"
        );

        // Send a request that will cache realm credentials.
        let mut realm_url = base_url.clone();
        realm_url.set_username(username).unwrap();
        assert_eq!(
            client.get(realm_url.clone()).send().await?.status(),
            200,
            "The first realm request with a username will succeed"
        );

        let mut url = index_url.clone();
        url.set_username(username).unwrap();
        assert_eq!(
            client.get(url.clone()).send().await?.status(),
            200,
            "A request with the same username and realm for a URL will use the realm if there is no index-specific password"
        );
        assert_eq!(
            client
                .get(base_url.join("prefix_1/foo")?)
                .send()
                .await?
                .status(),
            200,
            "Requests to other paths with that prefix will also succeed"
        );

        Ok(())
    }

    fn indexes_for(url: &Url, policy: AuthPolicy) -> Indexes {
        let mut url = DisplaySafeUrl::from(url.clone());
        url.set_password(None).ok();
        url.set_username("").ok();
        Indexes::from_indexes(vec![Index {
            url: url.clone(),
            root_url: url.clone(),
            auth_policy: policy,
        }])
    }

    /// With the "always" auth policy, requests should succeed on
    /// authenticated requests with the correct credentials.
    #[test(tokio::test)]
    async fn test_auth_policy_always_with_credentials() -> Result<(), Error> {
        let username = "user";
        let password = "password";

        let server = start_test_server(username, password).await;

        let base_url = Url::parse(&server.uri())?;

        let indexes = indexes_for(&base_url, AuthPolicy::Always);
        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_indexes(indexes),
            )
            .build();

        Mock::given(method("GET"))
            .and(path_regex("/*"))
            .and(basic_auth(username, password))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some(password)).unwrap();
        assert_eq!(client.get(url).send().await?.status(), 200);

        assert_eq!(
            client
                .get(format!("{}/foo", server.uri()))
                .send()
                .await?
                .status(),
            200,
            "Requests can be to different paths with index URL as prefix"
        );

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some("invalid")).unwrap();
        assert_eq!(
            client.get(url).send().await?.status(),
            401,
            "Incorrect credentials should fail"
        );

        Ok(())
    }

    /// With the "always" auth policy, requests should fail if only
    /// unauthenticated requests are supported.
    #[test(tokio::test)]
    async fn test_auth_policy_always_unauthenticated() -> Result<(), Error> {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path_regex("/*"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let base_url = Url::parse(&server.uri())?;

        let indexes = indexes_for(&base_url, AuthPolicy::Always);
        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_indexes(indexes),
            )
            .build();

        // Unauthenticated requests are not allowed.
        assert!(matches!(
            client.get(server.uri()).send().await,
            Err(reqwest_middleware::Error::Middleware(_))
        ),);

        Ok(())
    }

    /// With the "never" auth policy, requests should fail if
    /// an endpoint requires authentication.
    #[test(tokio::test)]
    async fn test_auth_policy_never_with_credentials() -> Result<(), Error> {
        let username = "user";
        let password = "password";

        let server = start_test_server(username, password).await;
        let base_url = Url::parse(&server.uri())?;

        Mock::given(method("GET"))
            .and(path_regex("/*"))
            .and(basic_auth(username, password))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let indexes = indexes_for(&base_url, AuthPolicy::Never);
        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_indexes(indexes),
            )
            .build();

        let mut url = base_url.clone();
        url.set_username(username).unwrap();
        url.set_password(Some(password)).unwrap();

        assert_eq!(
            client
                .get(format!("{}/foo", server.uri()))
                .send()
                .await?
                .status(),
            401,
            "Requests should not be completed if credentials are required"
        );

        Ok(())
    }

    /// With the "never" auth policy, requests should succeed if
    /// unauthenticated requests succeed.
    #[test(tokio::test)]
    async fn test_auth_policy_never_unauthenticated() -> Result<(), Error> {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path_regex("/*"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let base_url = Url::parse(&server.uri())?;

        let indexes = indexes_for(&base_url, AuthPolicy::Never);
        let client = test_client_builder()
            .with(
                AuthMiddleware::new()
                    .with_cache(CredentialsCache::new())
                    .with_indexes(indexes),
            )
            .build();

        assert_eq!(
            client.get(server.uri()).send().await?.status(),
            200,
            "Requests should succeed if unauthenticated requests can succeed"
        );

        Ok(())
    }

    #[test]
    fn test_tracing_url() {
        // No credentials
        let req = create_request("https://pypi-proxy.fly.dev/basic-auth/simple");
        assert_eq!(
            tracing_url(&req, None),
            DisplaySafeUrl::parse("https://pypi-proxy.fly.dev/basic-auth/simple").unwrap()
        );

        let creds = Credentials::Basic {
            username: Username::new(Some(String::from("user"))),
            password: None,
        };
        let req = create_request("https://pypi-proxy.fly.dev/basic-auth/simple");
        assert_eq!(
            tracing_url(&req, Some(&creds)),
            DisplaySafeUrl::parse("https://user@pypi-proxy.fly.dev/basic-auth/simple").unwrap()
        );

        let creds = Credentials::Basic {
            username: Username::new(Some(String::from("user"))),
            password: Some(Password::new(String::from("password"))),
        };
        let req = create_request("https://pypi-proxy.fly.dev/basic-auth/simple");
        assert_eq!(
            tracing_url(&req, Some(&creds)),
            DisplaySafeUrl::parse("https://user:password@pypi-proxy.fly.dev/basic-auth/simple")
                .unwrap()
        );
    }

    fn create_request(url: &str) -> Request {
        Request::new(Method::GET, Url::parse(url).unwrap())
    }
}
