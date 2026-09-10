//! Reading a listing that will not fit in one response, and giving up on one
//! that never arrives.
//!
//! Every listing in this tool used to be a single request that asked for
//! everything. That works until it does not: a cluster with ten thousand pods
//! answers a bare `GET /api/v1/pods` with a response measured in tens of
//! megabytes, which the API server has to hold in memory and we then have to
//! parse before a single row is drawn. Kubernetes' answer is `limit` and
//! `continue` — ask for [`SIZE`] objects at a time and hand the token back for
//! the next page — and this module is that loop.
//!
//! The loop is four lines of I/O in [`collect`]. Everything that decides
//! whether there is another page to ask for lives in [`Listing`], over a page
//! of items and a token, so a three-page listing is a fixture rather than a
//! cluster somebody has to grow.
//!
//! [`Budget`] rides along because it answers the other half of the same
//! question. A listing that pages is several requests rather than one, so "how
//! long may this take?" has to be asked of each request rather than of the
//! command: a cluster big enough to page is exactly the one whose listing
//! legitimately takes a while, and a budget spent by the whole command would
//! punish it for its size rather than for being unreachable.

use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use kube::api::{Api, ListParams};
use serde::de::DeserializeOwned;

use crate::format;
use crate::progress::Task;

/// How many objects one request asks for.
///
/// `kubectl`'s own chunk size. Large enough that an ordinary cluster is still
/// one round trip, small enough that a very large one is not one enormous
/// response — which is the whole point of asking in chunks.
pub const SIZE: u32 = 500;

/// What can go wrong reading a listing.
///
/// The API failures are `kube`'s own and are passed through untouched;
/// [`TimedOut`] is ours, and is the case `kube` has no opinion about because
/// waiting forever is a perfectly good thing for a library to do and a terrible
/// thing for a command-line tool to do.
///
/// [`TimedOut`]: Error::TimedOut
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The cluster answered, and the answer was a failure.
    #[error("{0}")]
    Api(#[from] kube::Error),

    /// The cluster did not answer at all within the budget.
    #[error("no answer within {}", format::exact_duration(*limit))]
    TimedOut { limit: Duration },
}

impl Error {
    /// The HTTP status behind this failure, when the cluster got as far as
    /// sending one.
    ///
    /// The status code is what decides the advice a user reads — a `404` from
    /// the aggregation layer means "install metrics-server", a `410` on a paged
    /// listing means "that took too long, run it again" — so it is read here
    /// rather than matched for at each call site through two layers of enum.
    #[must_use]
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Api(kube::Error::Api(status)) => Some(status.code),
            _ => None,
        }
    }
}

/// How long one request to the API server may take.
///
/// A per-request budget rather than a per-command one — see the module
/// documentation for why paging forces that choice. `None` waits for as long as
/// it takes, which is what `--timeout 0` asks for and what every version of
/// this tool did before the flag existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget(Option<Duration>);

impl Default for Budget {
    /// Thirty seconds.
    ///
    /// The default is a real number rather than "wait forever" because the
    /// failure it exists for is silent: a private EKS endpoint reached from
    /// outside its VPC does not refuse the connection, it simply never answers,
    /// and the tool sat there until somebody pressed Ctrl-C. Thirty seconds is
    /// long enough that a busy API server is not cut off mid-answer and short
    /// enough that a wrong network is a sentence rather than a hang.
    fn default() -> Self {
        Self(Some(Duration::from_secs(30)))
    }
}

impl Budget {
    /// A budget of exactly this long.
    #[must_use]
    pub fn of(limit: Duration) -> Self {
        Self(Some(limit))
    }

    /// No limit: wait for as long as the cluster takes.
    #[must_use]
    pub fn unlimited() -> Self {
        Self(None)
    }

    /// How long this allows, or `None` if it allows forever.
    #[must_use]
    pub fn limit(self) -> Option<Duration> {
        self.0
    }

    /// Run one request, failing it if the budget runs out first.
    ///
    /// The future is dropped when the budget expires, which is what cancels the
    /// request rather than leaving it running behind a message saying it was
    /// abandoned.
    pub async fn wrap<T>(
        self,
        request: impl Future<Output = Result<T, kube::Error>>,
    ) -> Result<T, Error> {
        let Some(limit) = self.0 else {
            return Ok(request.await?);
        };

        match tokio::time::timeout(limit, request).await {
            Ok(answer) => Ok(answer?),
            Err(_) => Err(Error::TimedOut { limit }),
        }
    }
}

impl fmt::Display for Budget {
    /// The spelling [`Budget::from_str`] would read back, so a budget can be
    /// printed into advice about what to type next.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(limit) => f.write_str(&format::exact_duration(limit)),
            None => f.write_str("0"),
        }
    }
}

/// Why a `--timeout` could not be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    /// Not a number, or not a unit we know.
    #[error(
        "not a length of time; give a number of seconds, or a number with a unit — \
         `30s`, `500ms`, `2m`. `0` waits for as long as the cluster takes."
    )]
    Unreadable,

    /// A number we can read and cannot wait for.
    #[error("longer than this tool can wait; `0` already means \"for as long as it takes\".")]
    TooLong,
}

impl FromStr for Budget {
    type Err = ParseError;

    /// `30s`, `500ms`, `2m`, `1h`, or a bare number of seconds.
    ///
    /// Deliberately narrower than Go's duration grammar, which `kubectl`
    /// accepts: a compound `1m30s` is the only thing missing, and supporting it
    /// would mean [`fmt::Display`] could print a spelling with two units in it,
    /// which is the one property the round trip depends on not happening.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let input = input.trim();
        let digits = input
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(input.len());
        let (count, unit) = input.split_at(digits);

        let count: u64 = count.parse().map_err(|_| ParseError::Unreadable)?;
        let millis = match unit.trim() {
            "ms" => count,
            // A bare number is seconds, as `kubectl --request-timeout` reads it.
            "" | "s" => count.checked_mul(1_000).ok_or(ParseError::TooLong)?,
            "m" => count.checked_mul(60_000).ok_or(ParseError::TooLong)?,
            "h" => count.checked_mul(3_600_000).ok_or(ParseError::TooLong)?,
            _ => return Err(ParseError::Unreadable),
        };

        // Zero is not a zero-length budget — nothing would ever finish — it is
        // how every duration flag in this corner of the world spells "no limit".
        Ok(if millis == 0 {
            Self::unlimited()
        } else {
            Self::of(Duration::from_millis(millis))
        })
    }
}

/// What the continue token on a finished page says to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Ask again, carrying this token.
    Page(String),

    /// That was the last page.
    Done,

    /// The server handed back the token it was given, so asking again would
    /// fetch the same page for ever. Kubernetes does not do this; the case
    /// exists because the alternative to noticing it is a command that never
    /// returns.
    Stalled,
}

/// Decide what a finished page says about the next one.
///
/// `sent` is the token the request carried and `received` is the token the
/// answer came back with. A server with nothing more to say sends no token, or
/// an empty one — both spellings appear in the wild, and both mean the same
/// thing.
#[must_use]
pub fn next(sent: Option<&str>, received: Option<&str>) -> Next {
    match received.map(str::trim).filter(|token| !token.is_empty()) {
        None => Next::Done,
        Some(token) if Some(token) == sent => Next::Stalled,
        Some(token) => Next::Page(token.to_owned()),
    }
}

/// The pages of one listing, accumulated.
///
/// Split out of [`collect`] so the paging rules are testable without a cluster:
/// everything here is a pure function over pages that arrived and the token
/// each carried.
#[derive(Debug)]
pub struct Listing<K> {
    items: Vec<K>,
    /// The continue token the last request carried, kept so a server that
    /// echoes it back is spotted rather than followed.
    sent: Option<String>,
}

impl<K> Default for Listing<K> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            sent: None,
        }
    }
}

impl<K> Listing<K> {
    /// A listing with no pages in it yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The parameters the next request should carry.
    ///
    /// `base` is the caller's own filtering — a label selector, a field
    /// selector — and every page repeats it: a continue token resumes *this*
    /// listing, and the server rejects one whose query has changed underneath
    /// it.
    #[must_use]
    pub fn params(&self, base: &ListParams) -> ListParams {
        let params = base.clone().limit(SIZE);
        match &self.sent {
            Some(token) => params.continue_token(token),
            None => params,
        }
    }

    /// Take one page, and say whether to ask for another.
    pub fn absorb(&mut self, page: impl IntoIterator<Item = K>, token: Option<&str>) -> Next {
        self.items.extend(page);

        let step = next(self.sent.as_deref(), token);
        if let Next::Page(token) = &step {
            self.sent = Some(token.clone());
        }
        step
    }

    /// Everything the pages held, in the order they arrived.
    #[must_use]
    pub fn finish(self) -> Vec<K> {
        self.items
    }
}

/// Read a listing to its end, a page at a time.
///
/// The only function here that touches the network. `base` carries the caller's
/// own filtering; the paging parameters are added on top of it per page.
///
/// `task` is where this listing says how far it has got — one line on stderr,
/// counting objects as the pages land, for a command whose user is watching it
/// happen. It is taken by value because it is *this* listing's step and ends
/// with it: dropping it takes the listing off the line, on the error path
/// below as surely as on the way out of the loop. Every caller that is not a
/// one-shot command in a terminal passes [`Task::default`], which does nothing
/// at all — see [`crate::progress`].
pub async fn collect<K>(
    api: &Api<K>,
    base: &ListParams,
    budget: Budget,
    task: Task,
) -> Result<Vec<K>, Error>
where
    K: Clone + DeserializeOwned + fmt::Debug,
{
    let mut listing = Listing::new();

    loop {
        // Ticked rather than plainly awaited: one page is allowed to take the
        // whole budget, and a listing whose first page is slow is exactly the
        // silence this is here to end.
        let page = task
            .tick(budget.wrap(api.list(&listing.params(base))))
            .await?;
        task.advance(page.items.len());

        match listing.absorb(page.items, page.metadata.continue_.as_deref()) {
            Next::Page(_) => {}
            Next::Done => return Ok(listing.finish()),
            Next::Stalled => {
                // Not fatal: the pages that did arrive are real, and half a
                // listing with a warning beside it beats no listing at all.
                tracing::warn!(
                    "the cluster repeated its page marker, so this listing may be short"
                );
                return Ok(listing.finish());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::io::{BufRead as _, Write as _};

    use k8s_openapi::api::core::v1::Node;

    use crate::progress::{Progress, Recorder};

    use super::*;

    /// Three pages of a listing, as the API server hands them over: the first
    /// two carry a token for the next, and the last carries nothing.
    fn three_pages() -> Listing<&'static str> {
        let mut listing = Listing::new();

        assert_eq!(
            listing.absorb(["a", "b"], Some("token-2")),
            Next::Page("token-2".to_owned())
        );
        assert_eq!(
            listing.absorb(["c", "d"], Some("token-3")),
            Next::Page("token-3".to_owned())
        );
        assert_eq!(listing.absorb(["e"], None), Next::Done);

        listing
    }

    #[test]
    fn a_listing_is_every_page_in_the_order_they_arrived() {
        assert_eq!(three_pages().finish(), vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn each_request_carries_the_token_the_page_before_it_returned() {
        let base = ListParams::default();
        let mut listing: Listing<&str> = Listing::new();

        // Nothing to resume from on the first request; a continue token there
        // would be a token the server never issued.
        assert_eq!(listing.params(&base).continue_token, None);
        assert_eq!(listing.params(&base).limit, Some(SIZE));

        listing.absorb(["a"], Some("token-2"));
        assert_eq!(
            listing.params(&base).continue_token.as_deref(),
            Some("token-2")
        );
    }

    #[test]
    fn every_page_repeats_the_callers_own_filtering() {
        // A continue token resumes one particular listing, and a server asked
        // to resume it under a different selector rejects the request.
        let base = ListParams::default()
            .labels("app=api")
            .fields("status.phase!=Running");
        let mut listing: Listing<&str> = Listing::new();
        listing.absorb(["a"], Some("token-2"));

        let params = listing.params(&base);
        assert_eq!(params.label_selector.as_deref(), Some("app=api"));
        assert_eq!(
            params.field_selector.as_deref(),
            Some("status.phase!=Running")
        );
        assert_eq!(params.continue_token.as_deref(), Some("token-2"));
    }

    #[test]
    fn a_listing_that_fits_in_one_page_is_one_request() {
        // The ordinary cluster: the change must cost it nothing.
        let mut listing = Listing::new();
        assert_eq!(listing.absorb(["only"], None), Next::Done);
        assert_eq!(listing.finish(), vec!["only"]);
    }

    #[test]
    fn an_empty_listing_ends_immediately_with_nothing_in_it() {
        let mut listing: Listing<&str> = Listing::new();
        assert_eq!(listing.absorb([], None), Next::Done);
        assert!(listing.finish().is_empty());
    }

    #[test]
    fn an_empty_continue_token_ends_the_listing() {
        // Some servers send `"continue": ""` rather than omitting the field,
        // and following that as a token asks for a page nobody has.
        assert_eq!(next(None, Some("")), Next::Done);
        assert_eq!(next(None, Some("   ")), Next::Done);
        assert_eq!(next(None, None), Next::Done);
    }

    #[test]
    fn a_repeated_token_stops_the_listing_rather_than_looping_for_ever() {
        assert_eq!(next(Some("token-2"), Some("token-2")), Next::Stalled);

        let mut listing = Listing::new();
        listing.absorb(["a"], Some("token-2"));
        assert_eq!(listing.absorb(["a"], Some("token-2")), Next::Stalled);
    }

    #[test]
    fn a_fresh_token_that_matches_no_earlier_one_is_followed() {
        assert_eq!(
            next(Some("token-2"), Some("token-3")),
            Next::Page("token-3".to_owned())
        );
    }

    #[tokio::test]
    async fn a_request_that_answers_in_time_is_left_alone() {
        let answer = Budget::of(Duration::from_secs(30))
            .wrap(async { Ok::<_, kube::Error>(7) })
            .await
            .unwrap();

        assert_eq!(answer, 7);
    }

    #[tokio::test]
    async fn a_request_that_never_answers_fails_with_the_budget_it_overran() {
        // Five milliseconds against an hour: the test takes five milliseconds,
        // because the budget expiring drops the request rather than waiting it
        // out.
        let budget = Budget::of(Duration::from_millis(5));
        let hang = async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok::<(), kube::Error>(())
        };

        let error = budget.wrap(hang).await.expect_err("the budget was 5ms");

        assert!(matches!(error, Error::TimedOut { limit } if limit == Duration::from_millis(5)));
        assert!(error.to_string().contains("5ms"), "{error}");
    }

    #[tokio::test]
    async fn an_unlimited_budget_never_gives_up() {
        // `--timeout 0`: the behaviour every version of this tool had before
        // the flag existed.
        let answer = Budget::unlimited()
            .wrap(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok::<_, kube::Error>("patient")
            })
            .await
            .unwrap();

        assert_eq!(answer, "patient");
    }

    #[test]
    fn a_budget_reads_seconds_by_default_and_takes_a_unit() {
        for (input, expected) in [
            ("30", Duration::from_secs(30)),
            ("30s", Duration::from_secs(30)),
            (" 45s ", Duration::from_secs(45)),
            ("500ms", Duration::from_millis(500)),
            ("2m", Duration::from_secs(120)),
            ("1h", Duration::from_secs(3600)),
        ] {
            assert_eq!(
                Budget::from_str(input).unwrap(),
                Budget::of(expected),
                "--timeout {input}"
            );
        }
    }

    #[test]
    fn zero_means_wait_for_as_long_as_it_takes() {
        for input in ["0", "0s", "0ms", "0m"] {
            assert_eq!(Budget::from_str(input).unwrap(), Budget::unlimited());
            assert_eq!(Budget::from_str(input).unwrap().limit(), None);
        }
    }

    #[test]
    fn a_budget_that_is_not_a_length_of_time_says_what_one_looks_like() {
        for input in ["", "soon", "30 seconds", "5x", "-1", "1.5s", "s"] {
            let error = Budget::from_str(input).expect_err("not a duration");

            assert_eq!(error, ParseError::Unreadable, "--timeout {input}");
            assert!(error.to_string().contains("30s"), "{error}");
            assert!(error.to_string().contains('0'), "{error}");
        }
    }

    #[test]
    fn a_budget_nobody_could_wait_out_is_rejected_rather_than_wrapped() {
        let error = Budget::from_str("18446744073709551615m").expect_err("that is a long time");

        assert_eq!(error, ParseError::TooLong);
        assert!(error.to_string().contains('0'), "{error}");
    }

    #[test]
    fn a_budget_prints_the_spelling_it_would_read_back() {
        // The property the "allow longer: `--timeout 1m`" advice rests on.
        for input in ["30s", "500ms", "2m", "1h", "0", "90s"] {
            let budget = Budget::from_str(input).unwrap();
            let printed = budget.to_string();

            assert_eq!(
                Budget::from_str(&printed).unwrap(),
                budget,
                "--timeout {input} printed as {printed}"
            );
        }
    }

    #[test]
    fn the_default_budget_is_thirty_seconds() {
        // Named here rather than only in the flag definition, because the
        // number is documented in the README and in `--help`.
        assert_eq!(Budget::default(), Budget::of(Duration::from_secs(30)));
        assert_eq!(Budget::default().to_string(), "30s");
    }

    // --- The loop itself, against a socket rather than a cluster ----------

    /// One page of a node listing as the API server sends it.
    fn node_page(names: &[&str], next: Option<&str>) -> String {
        let items: Vec<String> = names
            .iter()
            .map(|name| format!(r#"{{"metadata":{{"name":"{name}"}}}}"#))
            .collect();
        let carry = match next {
            Some(token) => format!(r#","continue":"{token}""#),
            None => String::new(),
        };
        format!(
            r#"{{"kind":"NodeList","apiVersion":"v1","metadata":{{"resourceVersion":"1"{carry}}},"items":[{}]}}"#,
            items.join(",")
        )
    }

    /// Answer `pages` in order, one request each, and stop.
    ///
    /// A real socket rather than a fake `Api`, because the claim being tested
    /// is about the *loop*: that each page's arrival is counted as it lands,
    /// which is a property of `collect` and not of anything either side of
    /// it. It is still a fixture — the bodies are canned, nothing resolves a
    /// name, and no credential is involved — and it is a plain OS thread with
    /// blocking reads, so it costs the runtime nothing.
    ///
    /// `connection: close` on every answer, so the next page arrives as a new
    /// connection this loop can simply accept again.
    fn serve(pages: Vec<String>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            for body in pages {
                let Ok((mut socket, _)) = listener.accept() else {
                    return;
                };

                // Drain the request; nothing here depends on what it says
                // beyond its having finished arriving.
                let Ok(peer) = socket.try_clone() else { return };
                let mut request = std::io::BufReader::new(peer);
                let mut line = String::new();
                while request.read_line(&mut line).unwrap_or(0) > 0 {
                    if line == "\r\n" {
                        break;
                    }
                    line.clear();
                }

                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     content-type: application/json\r\n\
                     content-length: {}\r\n\
                     connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes());
                let _ = socket.flush();
            }
        });

        url
    }

    /// Read a node listing from `url` through [`collect`], reporting into
    /// `recorder`.
    fn read_nodes(url: String, recorder: &Recorder) -> Result<Vec<Node>, anyhow::Error> {
        let recorder = recorder.clone();
        crate::commands::block_on(async move {
            let client = kube::Client::try_from(kube::Config::new(url.parse()?))?;
            let api: Api<Node> = Api::all(client);
            let progress = Progress::to(Box::new(recorder), 80);
            Ok(collect(
                &api,
                &ListParams::default(),
                Budget::default(),
                progress.reading("nodes"),
            )
            .await?)
        })
    }

    #[test]
    fn a_paged_listing_counts_its_objects_onto_the_line_as_they_arrive() {
        // The acceptance criterion for the counter, end to end: three nodes
        // over two pages, and the line has to move when the second lands
        // rather than only when the whole listing is done.
        let recorder = Recorder::default();
        let url = serve(vec![
            node_page(&["node-a", "node-b"], Some("token-2")),
            node_page(&["node-c"], None),
        ]);

        let nodes = read_nodes(url, &recorder).unwrap();
        let written = recorder.written();

        assert_eq!(nodes.len(), 3);
        assert!(
            written.contains("reading 2 nodes…"),
            "the first page was never counted: {written:?}"
        );
        assert!(
            written.contains("reading 3 nodes…"),
            "the second page was never counted: {written:?}"
        );
    }

    #[test]
    fn a_finished_listing_leaves_nothing_on_the_line_for_the_table_to_land_on() {
        // The other half of the acceptance criterion. The table is printed the
        // moment `collect` returns, so anything still on that row would have a
        // header written across it.
        let recorder = Recorder::default();
        let url = serve(vec![node_page(&["node-a"], None)]);

        read_nodes(url, &recorder).unwrap();

        assert!(
            recorder.is_erased(),
            "left behind: {:?}",
            recorder.written()
        );
    }

    #[test]
    fn a_listing_that_never_answers_erases_its_line_before_the_error_is_printed() {
        // A socket that accepts and says nothing, under a budget short enough
        // to give up on it. The message goes to stderr, where the progress
        // line is, so an unerased line would have the error written over it.
        let recorder = Recorder::default();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        let error = {
            let recorder = recorder.clone();
            crate::commands::block_on(async move {
                let client = kube::Client::try_from(kube::Config::new(url.parse()?))?;
                let api: Api<Node> = Api::all(client);
                let progress = Progress::to(Box::new(recorder), 80);
                let outcome: Result<Vec<Node>, Error> = collect(
                    &api,
                    &ListParams::default(),
                    Budget::of(Duration::from_millis(250)),
                    progress.reading("nodes"),
                )
                .await;
                Ok(outcome.expect_err("a socket that says nothing is not a listing"))
            })
            .unwrap()
        };

        assert!(matches!(error, Error::TimedOut { .. }), "{error}");
        assert!(
            recorder.is_erased(),
            "left behind: {:?}",
            recorder.written()
        );
        drop(listener);
    }
}
