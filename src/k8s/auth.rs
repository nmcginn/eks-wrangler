//! Keeping a credential helper's token fresh for as long as a client lives.
//!
//! [`crate::k8s::exec`] runs the helper once and hands back a token that is
//! good for fifteen minutes. A listing on a large cluster, or a dashboard left
//! open over lunch, outlives that. [`Keeper`] holds the token and runs the
//! helper again when it has to, and [`Authorise`] is the `tower` layer on the
//! client that asks it for a token on every request.
//!
//! "When it has to" is two rules, one before a request and one after:
//!
//! - **Before:** a token inside the last minute of its `expirationTimestamp`
//!   ([`exec::due`]) is replaced before it is sent, so the request that would
//!   have been refused is never made.
//! - **After:** a `401` marks the token that was refused as stale, so the next
//!   request fetches another rather than sending the same one again. That is
//!   the case the clock cannot see: a token with no expiry, or one revoked
//!   early. [`crate::k8s::page::collect`] is the caller that makes use of it,
//!   by asking once more for the page that was refused.
//!
//! Only tokens are kept this way. A client certificate is part of the TLS
//! connection rather than of each request, so a new one needs a new client;
//! it is held for the life of the client exactly as `kube` held it.
//!
//! The refresh happens inside the request, and spends the request's own
//! deadline — see [`crate::k8s::page::deadline`]. A refresh that stalls ends
//! at the same instant the request would have, and it is killed rather than
//! abandoned because the future that owns the child is dropped.
//!
//! A command-line listing builds one client and one keeper. The dashboard
//! builds a client per fetch, so its keepers live in a [`Store`] that every
//! fetch shares: one token per context for the whole session, refreshed by
//! whichever fetch finds it due, and replaced by `L` with one the helper
//! printed in the foreground (decision 120). A [`Store`] cannot be made to
//! let a helper prompt; that is the guarantee the dashboard's fetchers are
//! built on.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, PoisonError};
use std::task::{Context, Poll};

use http::header::AUTHORIZATION;
use http::{HeaderValue, Request, Response, StatusCode};
use k8s_openapi::jiff::Timestamp;
use kube::client::Body;
use kube::config::AuthInfo;
use tokio::sync::Mutex;
use tower::{BoxError, Layer, Service};

use crate::k8s::exec::{self, Credential, Prompt, Secret};
use crate::k8s::page::{self, Budget};

/// The token a client is sending, and what it takes to get another.
///
/// Cheap to clone: every clone shares one token, so a refresh made for one
/// request is the token every other request sends after it.
#[derive(Clone)]
pub struct Keeper {
    held: Arc<Mutex<Held>>,
    /// The context's `AuthInfo`, with the `exec` block the token came from.
    auth: Arc<AuthInfo>,
    /// Spent on a refresh only when the request has no deadline of its own.
    budget: Budget,
    /// Whether a refresh may prompt. Fixed when the keeper is made, so a
    /// refresh an hour into a dashboard runs exactly as the first fetch did.
    prompt: Prompt,
}

impl std::fmt::Debug for Keeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keeper").finish_non_exhaustive()
    }
}

struct Held {
    token: String,
    expires: Option<Timestamp>,
    /// Counts refreshes, so a `401` retires only the token it was sent with. A
    /// refusal that arrives after another request has already refreshed must
    /// not throw the new token away.
    generation: u64,
    stale: bool,
}

/// What [`Keeper::decide`] says to do before a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Before {
    /// Send the token held.
    Send,
    /// Run the helper first.
    Refresh,
}

impl Keeper {
    /// Keep `token`, which came from `auth`'s helper with this expiry, and
    /// run that helper under `prompt` when it needs replacing.
    #[must_use]
    pub fn new(
        token: String,
        expires: Option<Timestamp>,
        auth: AuthInfo,
        budget: Budget,
        prompt: Prompt,
    ) -> Self {
        Self {
            held: Arc::new(Mutex::new(Held {
                token,
                expires,
                generation: 0,
                stale: false,
            })),
            auth: Arc::new(auth),
            budget,
            prompt,
        }
    }

    /// Replace the token every clone of this keeper sends, with one that was
    /// fetched somewhere else — `L`'s foreground run of the helper.
    ///
    /// A new generation, so a `401` still in flight for the old token cannot
    /// retire this one.
    pub async fn seed(&self, token: String, expires: Option<Timestamp>) {
        let mut held = self.held.lock().await;
        held.token = token;
        held.expires = expires;
        held.generation += 1;
        held.stale = false;
    }

    /// Whether to refresh before sending: the whole policy, as a pure
    /// function.
    #[must_use]
    pub fn decide(expires: Option<Timestamp>, stale: bool, now: Timestamp) -> Before {
        if stale || exec::due(expires, now) {
            Before::Refresh
        } else {
            Before::Send
        }
    }

    /// The token to send now, refreshing it first if it is due, and the
    /// generation it belongs to.
    async fn current(&self) -> Result<(String, u64), exec::Error> {
        let mut held = self.held.lock().await;

        // Held across the helper on purpose. Three requests racing past the
        // expiry — `eks nodes` sends three at once — should run it once, and
        // the two that waited send the token the first one fetched.
        if Self::decide(held.expires, held.stale, Timestamp::now()) == Before::Refresh {
            let fresh = self.refresh().await?;
            let Secret::Token(token) = fresh.secret else {
                return Err(exec::Error::Unreadable {
                    command: crate::k8s::client::helper_command(&self.auth).unwrap_or_default(),
                    reason: "it sent a client certificate where it first sent a token, \
                             and a certificate cannot be changed on a connection that is already open"
                        .to_owned(),
                });
            };
            held.token = token;
            held.expires = fresh.expires;
            held.generation += 1;
            held.stale = false;
        }

        Ok((held.token.clone(), held.generation))
    }

    /// Run the helper under the request's deadline, or this keeper's budget
    /// when the request has none.
    async fn refresh(&self) -> Result<Credential, exec::Error> {
        tracing::debug!("refreshing the credential helper's token");
        let run = exec::run(&self.auth, self.prompt);

        let stalled = |limit| exec::Error::Stalled {
            command: crate::k8s::client::helper_command(&self.auth).unwrap_or_default(),
            limit,
        };

        match (page::deadline(), self.budget.limit()) {
            (Some(deadline), limit) => tokio::time::timeout_at(deadline, run)
                .await
                // The request's budget is the keeper's budget in every caller;
                // `--timeout` sets both. The `unwrap_or_default` covers the
                // one that is not — a request with a deadline on a keeper
                // built unlimited — and should not happen.
                .map_err(|_| stalled(limit.unwrap_or_default()))?,
            (None, Some(limit)) => tokio::time::timeout(limit, run)
                .await
                .map_err(|_| stalled(limit))?,
            (None, None) => run.await,
        }
    }

    /// The server refused the token from `generation`. Retire it, unless it has
    /// already been replaced.
    async fn refused(&self, generation: u64) {
        let mut held = self.held.lock().await;
        if held.generation == generation {
            held.stale = true;
        }
    }
}

/// The credentials the dashboard's fetches share, one per context.
///
/// Cheap to clone; every clone is the same store. Made with [`Store::new`],
/// which takes no [`Prompt`]: whatever builds a client through a store runs
/// its helper with [`Prompt::Never`], and nothing can ask it otherwise. The
/// only way a credential from a helper that *was* allowed to prompt gets in
/// is [`Store::seed`], handed one that was fetched in the foreground.
#[derive(Clone, Default)]
pub struct Store {
    kept: Arc<std::sync::Mutex<HashMap<String, Entry>>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

/// What a [`Store`] holds for a context, and the helper command it came from.
#[derive(Clone)]
struct Entry {
    command: String,
    kept: Kept,
}

/// A context's credential, in the form a client is built from.
#[derive(Clone)]
pub enum Kept {
    /// A token, with the keeper that refreshes it. Every client built from
    /// this shares the keeper.
    Token(Keeper),
    /// A client certificate and its key, both PEM, used until `expires` and
    /// then fetched again.
    Certificate {
        certificate: String,
        key: String,
        expires: Option<Timestamp>,
    },
}

impl std::fmt::Debug for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the secret, for the same reason `exec::Secret` writes its own.
        match self {
            Self::Token(_) => f.write_str("Token(..)"),
            Self::Certificate { .. } => f.write_str("Certificate(..)"),
        }
    }
}

impl Store {
    /// An empty store. Every helper run through it is [`Prompt::Never`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The policy every helper run for this store runs under. A method rather
    /// than a field, so there is nothing to set.
    #[must_use]
    pub fn prompt(&self) -> Prompt {
        Prompt::Never
    }

    /// What `context` can be built from without running its helper, if
    /// anything.
    ///
    /// `command` is the helper's command line now; an entry made from a
    /// different one — a kubeconfig edited while the dashboard was open — is
    /// not used. A certificate inside its last minute is not either: unlike a
    /// token, nothing on the request path can replace it.
    #[must_use]
    pub fn get(&self, context: &str, command: &str, now: Timestamp) -> Option<Kept> {
        let kept = self.lock();
        let entry = kept.get(context).filter(|entry| entry.command == command)?;
        match &entry.kept {
            Kept::Certificate { expires, .. } if exec::due(*expires, now) => None,
            kept => Some(kept.clone()),
        }
    }

    /// Remember what a context's client was built from.
    pub fn put(&self, context: &str, command: &str, kept: Kept) {
        self.lock().insert(
            context.to_owned(),
            Entry {
                command: command.to_owned(),
                kept,
            },
        );
    }

    /// Hand the store a credential the helper printed in the foreground, so
    /// the next fetch sends it instead of running the helper again without a
    /// terminal.
    ///
    /// A token goes into the keeper already there, when there is one for the
    /// same command, so a client already built from it — a log stream that is
    /// still open — sends the new token too. `auth` and `budget` make the
    /// keeper when there is not.
    pub async fn seed(
        &self,
        context: &str,
        command: &str,
        auth: AuthInfo,
        budget: Budget,
        credential: Credential,
    ) {
        let existing = match self.lock().get(context) {
            Some(Entry {
                command: was,
                kept: Kept::Token(keeper),
            }) if was == command => Some(keeper.clone()),
            _ => None,
        };

        let kept = match (credential.secret, existing) {
            (Secret::Token(token), Some(keeper)) => {
                keeper.seed(token, credential.expires).await;
                return;
            }
            (Secret::Token(token), None) => Kept::Token(Keeper::new(
                token,
                credential.expires,
                auth,
                budget,
                self.prompt(),
            )),
            (Secret::Certificate { certificate, key }, _) => Kept::Certificate {
                certificate,
                key,
                expires: credential.expires,
            },
        };
        self.put(context, command, kept);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // Nothing panics while holding this — every critical section is a
        // map lookup or insert — but a poisoned map is still a correct one.
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The `tower` layer that puts a [`Keeper`]'s token on every request.
#[derive(Debug, Clone)]
pub struct Authorise(pub Keeper);

impl<S> Layer<S> for Authorise {
    type Service = Authorised<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Authorised {
            inner: Arc::new(Mutex::new(inner)),
            keeper: self.0.clone(),
        }
    }
}

/// A service with [`Authorise`] applied.
///
/// The inner service is behind a lock because `kube`'s stack is not `Clone`
/// and the token has to be fetched — possibly by running a helper — *before*
/// the request is handed on. The lock is held only while the request is
/// dispatched, not while its answer is awaited, so requests still run
/// concurrently.
pub struct Authorised<S> {
    inner: Arc<Mutex<S>>,
    keeper: Keeper,
}

impl<S> std::fmt::Debug for Authorised<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authorised").finish_non_exhaustive()
    }
}

impl<S, B> Service<Request<Body>> for Authorised<S>
where
    S: Service<Request<Body>, Response = Response<B>> + Send + 'static,
    S::Error: Into<BoxError>,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = Response<B>;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Response<B>, BoxError>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Readiness is asked of the inner service under the lock, in `call`;
        // saying ready here costs nothing, since the future waits there.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut request: Request<Body>) -> Self::Future {
        let inner = Arc::clone(&self.inner);
        let keeper = self.keeper.clone();

        Box::pin(async move {
            // A helper's failure travels as our own error inside `kube`'s
            // `Service` variant; `page::Error::from` takes it back out, so it
            // is explained as a helper's failure and not as a network's.
            // Boxed straight into a `BoxError`: boxing it first and letting
            // `?` convert would box the box, and the downcast would miss it.
            let (token, generation) = keeper.current().await.map_err(BoxError::from)?;
            let value = HeaderValue::from_str(&format!("Bearer {token}"))?;
            request.headers_mut().insert(AUTHORIZATION, value.clone());

            let sent = {
                let mut inner = inner.lock().await;
                std::future::poll_fn(|cx| inner.poll_ready(cx))
                    .await
                    .map_err(Into::into)?;
                inner.call(request)
            };
            let response = sent.await.map_err(Into::into)?;

            if response.status() == StatusCode::UNAUTHORIZED {
                keeper.refused(generation).await;
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    #[test]
    fn a_token_with_time_left_is_sent_as_it_is() {
        assert_eq!(
            Keeper::decide(
                Some(at("2026-09-25T06:14:00Z")),
                false,
                at("2026-09-25T06:00:00Z")
            ),
            Before::Send
        );
    }

    #[test]
    fn a_token_about_to_expire_is_refreshed_before_it_is_sent() {
        assert_eq!(
            Keeper::decide(
                Some(at("2026-09-25T06:00:30Z")),
                false,
                at("2026-09-25T06:00:00Z")
            ),
            Before::Refresh
        );
    }

    #[test]
    fn a_refused_token_is_refreshed_however_long_it_claims_to_have_left() {
        assert_eq!(
            Keeper::decide(
                Some(at("2026-09-25T06:14:00Z")),
                true,
                at("2026-09-25T06:00:00Z")
            ),
            Before::Refresh
        );
        assert_eq!(
            Keeper::decide(None, true, at("2026-09-25T06:00:00Z")),
            Before::Refresh
        );
    }

    #[test]
    fn a_token_that_never_said_when_it_expires_is_kept_until_it_is_refused() {
        assert_eq!(
            Keeper::decide(None, false, at("2999-01-01T00:00:00Z")),
            Before::Send
        );
    }

    #[tokio::test]
    async fn a_refusal_of_a_token_already_replaced_does_not_retire_its_replacement() {
        // Two requests go out with generation 0; the first `401` comes back,
        // the next request refreshes to generation 1, and only then does the
        // second `401` arrive. Retiring generation 1 for it would run the
        // helper again for a token nobody has refused.
        let keeper = Keeper::new(
            "old".to_owned(),
            None,
            AuthInfo::default(),
            Budget::default(),
            Prompt::AsConfigured,
        );
        {
            let mut held = keeper.held.lock().await;
            held.token = "new".to_owned();
            held.generation = 1;
        }

        keeper.refused(0).await;
        assert!(!keeper.held.lock().await.stale);

        keeper.refused(1).await;
        assert!(keeper.held.lock().await.stale);
    }

    #[test]
    fn a_keeper_never_prints_its_token_when_debugged() {
        let keeper = Keeper::new(
            "secret-token".to_owned(),
            None,
            AuthInfo::default(),
            Budget::default(),
            Prompt::AsConfigured,
        );
        assert!(!format!("{keeper:?}").contains("secret-token"));
        assert!(!format!("{:?}", Authorise(keeper)).contains("secret-token"));
    }

    fn token(kept: Option<Kept>) -> Keeper {
        match kept {
            Some(Kept::Token(keeper)) => keeper,
            other => panic!("{other:?}"),
        }
    }

    fn keeper(token: &str) -> Keeper {
        Keeper::new(
            token.to_owned(),
            None,
            AuthInfo::default(),
            Budget::default(),
            Prompt::Never,
        )
    }

    #[tokio::test]
    async fn a_store_hands_every_fetch_for_a_context_the_same_keeper() {
        let store = Store::new();
        store.put("prod", "aws eks get-token", Kept::Token(keeper("t1")));

        let first = token(store.get("prod", "aws eks get-token", at("2026-09-25T06:00:00Z")));
        first.seed("t2".to_owned(), None).await;
        let second = token(store.get("prod", "aws eks get-token", at("2026-09-25T06:00:00Z")));

        assert_eq!(second.current().await.unwrap().0, "t2");
    }

    #[test]
    fn a_store_keeps_contexts_apart_and_forgets_a_helper_the_kubeconfig_no_longer_runs() {
        let store = Store::new();
        let now = at("2026-09-25T06:00:00Z");
        store.put(
            "prod",
            "aws eks get-token --cluster-name prod",
            Kept::Token(keeper("t")),
        );

        assert!(
            store
                .get("staging", "aws eks get-token --cluster-name prod", now)
                .is_none()
        );
        // Edited while the dashboard was open: the old token is not this
        // command's to send.
        assert!(
            store
                .get("prod", "aws eks get-token --cluster-name prod-2", now)
                .is_none()
        );
        assert!(
            store
                .get("prod", "aws eks get-token --cluster-name prod", now)
                .is_some()
        );
    }

    #[test]
    fn a_kept_certificate_is_used_until_its_last_minute_and_not_after() {
        let store = Store::new();
        store.put(
            "prod",
            "helper",
            Kept::Certificate {
                certificate: "cert".to_owned(),
                key: "key".to_owned(),
                expires: Some(at("2026-09-25T06:10:00Z")),
            },
        );

        assert!(
            store
                .get("prod", "helper", at("2026-09-25T06:00:00Z"))
                .is_some()
        );
        assert!(
            store
                .get("prod", "helper", at("2026-09-25T06:09:30Z"))
                .is_none()
        );
    }

    #[test]
    fn a_store_never_lets_a_helper_prompt() {
        // There is no constructor that takes a policy, and this is the one
        // it answers with; the fetcher tests check what a helper is given.
        assert_eq!(Store::new().prompt(), Prompt::Never);
        assert_eq!(Store::default().prompt(), Prompt::Never);
    }

    #[tokio::test]
    async fn seeding_a_store_reaches_clients_already_built_from_it() {
        // A log stream opened before `L` holds a clone of the keeper; it should
        // send the token `L` fetched, not the one that was refused.
        let store = Store::new();
        let open = keeper("refused");
        store.put("prod", "helper", Kept::Token(open.clone()));

        store
            .seed(
                "prod",
                "helper",
                AuthInfo::default(),
                Budget::default(),
                Credential {
                    secret: Secret::Token("from-l".to_owned()),
                    expires: None,
                },
            )
            .await;

        assert_eq!(open.current().await.unwrap().0, "from-l");
    }

    #[tokio::test]
    async fn a_refusal_of_the_token_l_replaced_does_not_retire_what_l_fetched() {
        let open = keeper("refused");
        let (_, generation) = open.current().await.unwrap();

        open.seed("from-l".to_owned(), None).await;
        open.refused(generation).await;

        assert!(!open.held.lock().await.stale);
    }

    #[tokio::test]
    async fn seeding_an_empty_store_keeps_the_credential_for_the_next_fetch() {
        let store = Store::new();
        let now = at("2026-09-25T06:00:00Z");

        store
            .seed(
                "prod",
                "helper",
                AuthInfo::default(),
                Budget::default(),
                Credential {
                    secret: Secret::Token("from-l".to_owned()),
                    expires: None,
                },
            )
            .await;
        let seeded = token(store.get("prod", "helper", now));
        assert_eq!(seeded.current().await.unwrap().0, "from-l");
        // And a refresh of it, later, is still the dashboard's kind.
        assert_eq!(seeded.prompt, Prompt::Never);

        store
            .seed(
                "prod",
                "helper",
                AuthInfo::default(),
                Budget::default(),
                Credential {
                    secret: Secret::Certificate {
                        certificate: "cert".to_owned(),
                        key: "key".to_owned(),
                    },
                    expires: None,
                },
            )
            .await;
        assert!(matches!(
            store.get("prod", "helper", now),
            Some(Kept::Certificate { .. })
        ));
    }

    #[test]
    fn a_kept_credential_never_prints_its_secret_when_debugged() {
        let certificate = Kept::Certificate {
            certificate: "cert-body".to_owned(),
            key: "key-body".to_owned(),
            expires: None,
        };
        let debugged = format!("{certificate:?} {:?}", Kept::Token(keeper("secret-token")));
        assert!(!debugged.contains("key-body"), "{debugged}");
        assert!(!debugged.contains("secret-token"), "{debugged}");
    }

    // --- End to end: a real helper, a real socket, no cluster ---------------
    //
    // The helper is `sh`, printing a token numbered by how many times it has
    // run (a counter in a file), so each request's `Authorization` header says
    // which run of the helper it came from. The "cluster" is a socket that
    // answers node listings and refuses the tokens a test tells it to.

    use std::io::{BufRead as _, Write as _};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex as StdMutex;
    use std::time::{Duration, Instant};

    use k8s_openapi::api::core::v1::Node;
    use kube::api::{Api, ListParams};

    use crate::cluster::ClusterView;
    use crate::k8s::client;
    use crate::progress::Progress;

    /// A helper that counts its runs in `dir/runs` and prints `token-N`,
    /// expiring at `expires` (or never, for `None`).
    fn counting_helper(dir: &Path, expires: Option<&str>) -> String {
        let expiry = expires
            .map(|at| format!(r#", \"expirationTimestamp\": \"{at}\""#))
            .unwrap_or_default();
        let runs = dir.join("runs");
        format!(
            r#"n=$(( $(cat {runs} 2>/dev/null || echo 0) + 1 )); echo $n > {runs}; echo "{{\"status\": {{\"token\": \"token-$n\"{expiry}}}}}""#,
            runs = runs.display()
        )
    }

    fn runs(dir: &Path) -> u32 {
        std::fs::read_to_string(dir.join("runs")).map_or(0, |text| text.trim().parse().unwrap())
    }

    fn kubeconfig(dir: &Path, server: &str, script: &str) -> Vec<PathBuf> {
        let yaml = format!(
            r"
apiVersion: v1
kind: Config
current-context: prod
clusters:
  - name: prod
    cluster:
      server: {server}
contexts:
  - name: prod
    context:
      cluster: prod
      user: prod
users:
  - name: prod
    user:
      exec:
        apiVersion: client.authentication.k8s.io/v1beta1
        command: sh
        args: ['-c', {script:?}]
        interactiveMode: Never
"
        );
        let path = dir.join("config");
        std::fs::write(&path, yaml).unwrap();
        vec![path]
    }

    fn view() -> ClusterView {
        ClusterView {
            context_name: "prod".to_owned(),
            display_name: "prod".to_owned(),
            region: Some("us-east-1".to_owned()),
            account_id: None,
            namespace: "default".to_owned(),
            is_current: true,
        }
    }

    fn node_page(names: &[&str], next: Option<&str>) -> String {
        let items: Vec<String> = names
            .iter()
            .map(|name| format!(r#"{{"metadata":{{"name":"{name}"}}}}"#))
            .collect();
        let carry = next
            .map(|token| format!(r#","continue":"{token}""#))
            .unwrap_or_default();
        format!(
            r#"{{"kind":"NodeList","apiVersion":"v1","metadata":{{"resourceVersion":"1"{carry}}},"items":[{}]}}"#,
            items.join(",")
        )
    }

    const UNAUTHORIZED: &str = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"Unauthorized","reason":"Unauthorized","code":401}"#;

    /// Serve node listings: the page a request asks for is chosen by its
    /// continue token (`none` → first), and `refuse` says which tokens get a
    /// `401` instead. Every `Authorization` header seen is recorded, in order.
    fn serve(
        pages: Vec<(Option<&'static str>, String)>,
        refuse: fn(&str) -> bool,
    ) -> (String, std::sync::Arc<StdMutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(StdMutex::new(Vec::new()));
        let record = std::sync::Arc::clone(&seen);

        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let Ok(mut socket) = socket else { return };
                let Ok(peer) = socket.try_clone() else { return };
                let mut request = std::io::BufReader::new(peer);

                let mut line = String::new();
                let mut target = String::new();
                let mut bearer = String::new();
                while request.read_line(&mut line).unwrap_or(0) > 0 {
                    if line == "\r\n" {
                        break;
                    }
                    if target.is_empty() {
                        target = line.clone();
                    }
                    if let Some(value) = line
                        .to_ascii_lowercase()
                        .strip_prefix("authorization: bearer ")
                    {
                        bearer = value.trim().to_owned();
                    }
                    line.clear();
                }
                record.lock().unwrap().push(bearer.clone());

                let body = if refuse(&bearer) {
                    ("401 Unauthorized", UNAUTHORIZED.to_owned())
                } else {
                    let page = pages
                        .iter()
                        .find(|(token, _)| match token {
                            Some(token) => target.contains(&format!("continue={token}")),
                            None => !target.contains("continue="),
                        })
                        .map(|(_, body)| body.clone())
                        .unwrap_or_default();
                    ("200 OK", page)
                };
                let response = format!(
                    "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.0,
                    body.1.len(),
                    body.1
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });

        (url, seen)
    }

    fn three_pages() -> Vec<(Option<&'static str>, String)> {
        vec![
            (None, node_page(&["a"], Some("p2"))),
            (Some("p2"), node_page(&["b"], Some("p3"))),
            (Some("p3"), node_page(&["c"], None)),
        ]
    }

    /// Connect through `client::connect` and read every node, as `eks nodes`
    /// would.
    fn list(paths: Vec<PathBuf>, budget: Budget) -> Result<Vec<String>, String> {
        crate::commands::block_on(async move {
            // Resolved and built in two steps rather than through `connect`,
            // to clear the proxy `kube` would otherwise take from
            // `HTTPS_PROXY`: the "cluster" is a socket on this machine.
            let mut config = client::resolve(&paths, &view()).await?;
            config.proxy_url = None;
            let client =
                match client::build(config, "prod (us-east-1)", budget, &Progress::none()).await {
                    Ok(client) => client,
                    Err(error) => return Ok(Err(error.to_string())),
                };
            let api: Api<Node> = Api::all(client);
            Ok(
                match page::collect(
                    &api,
                    &ListParams::default(),
                    budget,
                    crate::progress::Task::default(),
                )
                .await
                {
                    Ok(nodes) => Ok(nodes
                        .into_iter()
                        .filter_map(|node| node.metadata.name)
                        .collect()),
                    Err(error) => Err(client::explain(&error, "prod (us-east-1)")),
                },
            )
        })
        .unwrap()
    }

    #[test]
    fn building_a_client_runs_the_helper_once_not_once_per_use_of_it() {
        // `kube` ran it three times: for the TLS identity, the auth layer, and
        // the identity's expiry. Each run of `aws eks get-token` is an
        // interpreter starting up, on the path to the first row.
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(vec![(None, node_page(&["a"], None))], |_| false);
        let paths = kubeconfig(dir.path(), &url, &counting_helper(dir.path(), None));

        assert_eq!(list(paths, Budget::default()).unwrap(), ["a"]);

        assert_eq!(runs(dir.path()), 1);
        assert_eq!(*seen.lock().unwrap(), ["token-1"]);
    }

    #[test]
    fn a_token_that_is_refused_partway_through_a_listing_costs_the_page_not_the_listing() {
        // The acceptance criterion. `token-1` reads page one and is refused on
        // page two — revoked, or lapsed without an expiry to see coming. The
        // helper runs again, page two is asked for once more, and every node
        // arrives.
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(three_pages(), {
            // Accepted exactly once: the first request.
            static USED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            |token| token == "token-1" && USED.swap(true, std::sync::atomic::Ordering::SeqCst)
        });
        let paths = kubeconfig(dir.path(), &url, &counting_helper(dir.path(), None));

        assert_eq!(list(paths, Budget::default()).unwrap(), ["a", "b", "c"]);

        assert_eq!(runs(dir.path()), 2);
        assert_eq!(
            *seen.lock().unwrap(),
            ["token-1", "token-1", "token-2", "token-2"]
        );
    }

    #[test]
    fn a_token_about_to_expire_is_replaced_before_the_page_that_would_be_refused() {
        // An expiry in the past stands in for "inside its last minute": the
        // clock says it is due before every request, so every request carries a
        // token minted for it and none is refused.
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(three_pages(), |token| token == "token-1");
        let paths = kubeconfig(
            dir.path(),
            &url,
            &counting_helper(dir.path(), Some("2000-01-01T00:00:00Z")),
        );

        assert_eq!(list(paths, Budget::default()).unwrap(), ["a", "b", "c"]);

        assert_eq!(*seen.lock().unwrap(), ["token-2", "token-3", "token-4"]);
    }

    #[test]
    fn a_token_with_time_left_is_sent_on_every_page_without_running_the_helper_again() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(three_pages(), |_| false);
        let paths = kubeconfig(
            dir.path(),
            &url,
            &counting_helper(dir.path(), Some("2999-01-01T00:00:00Z")),
        );

        assert_eq!(list(paths, Budget::default()).unwrap(), ["a", "b", "c"]);

        assert_eq!(runs(dir.path()), 1);
        assert_eq!(*seen.lock().unwrap(), ["token-1", "token-1", "token-1"]);
    }

    #[test]
    fn a_fresh_token_the_cluster_still_refuses_is_explained_rather_than_retried_for_ever() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(three_pages(), {
            // `token-1` reads page one; after that nothing is accepted.
            static USED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            |token| token != "token-1" || USED.swap(true, std::sync::atomic::Ordering::SeqCst)
        });
        let paths = kubeconfig(dir.path(), &url, &counting_helper(dir.path(), None));

        let message = list(paths, Budget::default()).expect_err("token-2 is refused too");

        assert!(
            message.contains("prod (us-east-1) rejected your credentials"),
            "{message}"
        );
        // Page one, page two refused, page two once more with token-2, refused.
        assert_eq!(seen.lock().unwrap().len(), 3);
        assert_eq!(runs(dir.path()), 2);
    }

    #[test]
    fn a_refusal_on_the_first_page_is_not_asked_again() {
        // The token was minted a moment ago, for this command; the cluster
        // refusing it is the answer, and the login offer upstream is what
        // follows it — not a second wait for the same refusal.
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(three_pages(), |_| true);
        let paths = kubeconfig(dir.path(), &url, &counting_helper(dir.path(), None));

        let message = list(paths, Budget::default()).expect_err("everything is refused");

        assert!(message.contains("rejected your credentials"), "{message}");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(runs(dir.path()), 1);
    }

    /// Whether the process `pid` has gone — exited, or killed and waiting to
    /// be reaped, which is as gone as a process can be from outside it.
    fn gone(pid: &str) -> bool {
        let state = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&state.stdout);
        state.trim().is_empty() || state.trim().starts_with('Z')
    }

    /// Wait up to five seconds for `pid` to be gone; a `SIGKILL` is delivered
    /// asynchronously, so the very next instant may still see it.
    fn gone_soon(pid: &str) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if gone(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_refresh_that_stalls_partway_through_is_stopped_and_reported_as_the_helper() {
        // The first run answers with a token already due; the second — the
        // refresh before page one — records its pid and sleeps. The budget
        // runs out on it, and the message is about the helper, not the VPN.
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let script = format!(
            r#"if [ -e {runs} ]; then echo $$ > {pid}; exec sleep 30; fi; touch {runs}; echo '{{"status": {{"token": "t", "expirationTimestamp": "2000-01-01T00:00:00Z"}}}}'"#,
            runs = dir.path().join("runs").display(),
            pid = pid.display(),
        );
        let (url, _) = serve(three_pages(), |_| false);
        let paths = kubeconfig(dir.path(), &url, &script);

        let started = Instant::now();
        let message = list(paths, Budget::of(Duration::from_millis(500)))
            .expect_err("the refresh never answers");

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert!(
            message.contains("getting credentials for prod (us-east-1) took longer than 500ms"),
            "{message}"
        );
        assert!(message.contains("has been stopped"), "{message}");
        assert!(!message.contains("VPN"), "{message}");
        let pid = std::fs::read_to_string(pid).unwrap();
        assert!(
            gone_soon(&pid),
            "the stalled refresh {pid} is still running"
        );
    }

    #[test]
    fn a_helper_that_outlives_the_budget_while_connecting_is_killed_not_left_behind() {
        // The other roadmap entry this runner closes: `kube` could only stop
        // waiting for it, and the helper went on holding the terminal's stdin.
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let script = format!("echo $$ > {}; exec sleep 30", pid.display());
        let paths = kubeconfig(dir.path(), "http://127.0.0.1:9", &script);

        let message =
            list(paths, Budget::of(Duration::from_millis(500))).expect_err("it never answers");

        assert!(message.contains("has been stopped"), "{message}");
        let pid = std::fs::read_to_string(pid).unwrap();
        assert!(
            gone_soon(&pid),
            "the helper {pid} outlived the command that ran it"
        );
    }

    // --- The dashboard's store, end to end ------------------------------------

    /// One dashboard fetch through `store`: build a client the way the panes
    /// do and read every node.
    fn fetch(paths: &[PathBuf], store: &Store) -> Result<Vec<String>, String> {
        let paths = paths.to_vec();
        let store = store.clone();
        crate::commands::block_on(async move {
            let mut config = client::resolve(&paths, &view()).await?;
            config.proxy_url = None;
            let client = match client::build_kept(
                config,
                "prod (us-east-1)",
                Budget::default(),
                &store,
                "prod",
            )
            .await
            {
                Ok(client) => client,
                Err(error) => return Ok(Err(error.to_string())),
            };
            let api: Api<Node> = Api::all(client);
            Ok(
                match page::collect(
                    &api,
                    &ListParams::default(),
                    Budget::default(),
                    crate::progress::Task::default(),
                )
                .await
                {
                    Ok(nodes) => Ok(nodes
                        .into_iter()
                        .filter_map(|node| node.metadata.name)
                        .collect()),
                    Err(error) => Err(client::explain(&error, "prod (us-east-1)")),
                },
            )
        })
        .unwrap()
    }

    #[test]
    fn every_dashboard_fetch_sends_the_token_the_first_one_fetched() {
        // Before the store, each pane's fetch built its own client and ran
        // `aws eks get-token` again — an interpreter starting per pane, per
        // refresh — and there was nowhere for `L` to put a token.
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(vec![(None, node_page(&["a"], None))], |_| false);
        let paths = kubeconfig(
            dir.path(),
            &url,
            &counting_helper(dir.path(), Some("2999-01-01T00:00:00Z")),
        );
        let store = Store::new();

        for _ in 0..3 {
            assert_eq!(fetch(&paths, &store).unwrap(), ["a"]);
        }

        assert_eq!(runs(dir.path()), 1);
        assert_eq!(*seen.lock().unwrap(), ["token-1", "token-1", "token-1"]);
    }

    #[test]
    fn a_token_refused_in_one_fetch_is_replaced_in_the_next() {
        // The refusal retires the shared token, so the next refresh runs the
        // helper again rather than sending what was just refused.
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(vec![(None, node_page(&["a"], None))], |token| {
            token == "token-1"
        });
        let paths = kubeconfig(dir.path(), &url, &counting_helper(dir.path(), None));
        let store = Store::new();

        let message = fetch(&paths, &store).expect_err("token-1 is refused");
        assert!(message.contains("rejected your credentials"), "{message}");
        assert_eq!(fetch(&paths, &store).unwrap(), ["a"]);

        assert_eq!(runs(dir.path()), 2);
        assert_eq!(*seen.lock().unwrap(), ["token-1", "token-2"]);
    }

    #[test]
    fn the_next_fetch_after_l_sends_what_l_fetched_without_running_the_helper() {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = serve(vec![(None, node_page(&["a"], None))], |token| {
            token == "token-1"
        });
        let paths = kubeconfig(dir.path(), &url, &counting_helper(dir.path(), None));
        let store = Store::new();
        fetch(&paths, &store).expect_err("token-1 is refused");

        // What `credentials::retry_login` does once the foreground helper has
        // printed a credential.
        let auth = crate::commands::block_on(async {
            Ok(client::resolve(&paths, &view()).await?.auth_info)
        })
        .unwrap();
        let command = client::helper_command(&auth).unwrap();
        crate::commands::block_on(async {
            store
                .seed(
                    "prod",
                    &command,
                    auth.clone(),
                    Budget::default(),
                    Credential {
                        secret: Secret::Token("from-l".to_owned()),
                        expires: Some("2999-01-01T00:00:00Z".parse().unwrap()),
                    },
                )
                .await;
            Ok(())
        })
        .unwrap();

        assert_eq!(fetch(&paths, &store).unwrap(), ["a"]);
        assert_eq!(runs(dir.path()), 1);
        assert_eq!(*seen.lock().unwrap(), ["token-1", "from-l"]);
    }

    #[test]
    fn a_helper_that_wanted_to_ask_fails_a_dashboard_fetch_with_the_advice_to_press_l() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = exec::fixtures::recording_kubeconfig(dir.path(), "http://127.0.0.1:9", "exit 1");
        let path = dir.path().join("config");
        std::fs::write(&path, yaml).unwrap();

        let message = fetch(&[path], &Store::new()).expect_err("the helper exits 1");

        assert!(
            message.contains("may not ask you anything: Enter MFA code:"),
            "{message}"
        );
        assert!(message.contains("L runs `sh -c"), "{message}");
        let (interactive, stdin, _) = exec::fixtures::recorded(dir.path());
        assert!(!interactive);
        assert_eq!(stdin, "null");
    }
}
