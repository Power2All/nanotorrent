//! Short-lived capability tokens for the streaming endpoint.
//!
//! A media player cannot be handed the web interface's password. VLC opening a
//! playlist file will not prompt for credentials it was never given, and the
//! obvious alternative - writing `http://user:pass@host/...` into the `.m3u` -
//! leaves that password in clear text in a file that sits in the Downloads
//! folder until somebody deletes it, and on screen in any recording of it.
//!
//! So a token instead, and a deliberately weak one:
//!
//! * It authorises **one file of one torrent**. A token minted for episode one
//!   cannot read episode two, let alone `/api/settings`. It is a capability for
//!   a single read, not a session.
//! * It **slides**: valid for [`TTL`] from its last use rather than from issue.
//!   A film runs longer than any fixed window worth calling short-lived, and
//!   players re-request constantly while seeking; an unused token still dies on
//!   schedule, which is the case that matters.
//! * It lives in memory only. Restarting NanoTorrent invalidates every one, and
//!   nothing is written to disk to leak later.
//!
//! What it is not: a general-purpose API key. Nothing else accepts it, and it
//! is checked against one route in one middleware.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::RngExt;

/// How long a token survives after its last use.
///
/// Long enough that pausing a film for a cup of tea does not kill playback,
/// short enough that a token copied out of a browser's download history is
/// worthless by the time anyone finds it.
pub const TTL: Duration = Duration::from_secs(30 * 60);

/// The torrent and file a token is good for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Scope {
    /// Lowercased, because the info hash reaches this from a URL a person or a
    /// player may have retyped, and `AbC` and `abc` are the same torrent.
    hash: [u8; 40],
    index: usize,
}

impl Scope {
    fn new(hash: &str, index: usize) -> Option<Self> {
        let lower = hash.trim().to_ascii_lowercase();
        let bytes = lower.as_bytes();
        if bytes.len() != 40 || !lower.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let mut fixed = [0u8; 40];
        fixed.copy_from_slice(bytes);
        Some(Self { hash: fixed, index })
    }
}

/// The running server's token store, for callers that are not requests.
///
/// A plugin building a URL for a media player needs a token, and it is not
/// making an HTTP request to get one - it is inside the process already. This
/// is how it reaches the same store the middleware checks against.
///
/// None while the web interface is off, which is the whole answer to "can this
/// plugin stream": no server, no stream.
static LIVE: std::sync::OnceLock<Mutex<Option<std::sync::Arc<StreamTokens>>>> =
    std::sync::OnceLock::new();

fn live() -> &'static Mutex<Option<std::sync::Arc<StreamTokens>>> {
    LIVE.get_or_init(|| Mutex::new(None))
}

/// Called by the server as it starts.
pub fn publish(tokens: std::sync::Arc<StreamTokens>) {
    *live().lock().unwrap_or_else(|e| e.into_inner()) = Some(tokens);
}

/// Called by the server as it stops, so a plugin cannot mint tokens for a
/// server that is no longer listening.
pub fn unpublish() {
    *live().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Mint a token against the running server, if there is one.
pub fn issue_live(hash: &str, index: usize) -> Option<String> {
    live()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .issue(hash, index)
}

/// Live tokens, keyed by the token string itself.
#[derive(Default)]
pub struct StreamTokens {
    live: Mutex<HashMap<String, (Scope, Instant)>>,
}

impl StreamTokens {
    /// Mint a token good for one file of one torrent.
    ///
    /// `None` for a hash that is not 40 hex characters: a scope that cannot be
    /// compared later is a token that would authorise nothing, and minting one
    /// would only hide the caller's mistake.
    pub fn issue(&self, hash: &str, index: usize) -> Option<String> {
        let scope = Scope::new(hash, index)?;

        // 32 bytes from the OS generator, hex. Guessing one is not a threat
        // model anybody needs to think about at that width, and hex keeps it
        // safe to paste into a URL, a playlist and a log line.
        let mut raw = [0u8; 32];
        rand::rng().fill(&mut raw[..]);
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();

        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        // Swept here rather than on a timer: the map is only ever touched from
        // these two methods, so there is nowhere for dead entries to accumulate
        // unseen, and a background task for a handful of strings is not worth
        // its own shutdown path.
        let now = Instant::now();
        live.retain(|_, (_, last)| now.duration_since(*last) < TTL);
        live.insert(token.clone(), (scope, now));
        Some(token)
    }

    /// Is this token good for this file, right now? Using it extends its life.
    pub fn verify(&self, token: &str, hash: &str, index: usize) -> bool {
        let Some(scope) = Scope::new(hash, index) else {
            return false;
        };
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();

        let Some((held, last)) = live.get_mut(token) else {
            return false;
        };
        if now.duration_since(*last) >= TTL {
            live.remove(token);
            return false;
        }
        if *held != scope {
            // Deliberately NOT removed: a token used against the wrong file is
            // far more likely to be a stale tab than an attack, and dropping it
            // would let anyone who guesses a scope revoke someone's playback.
            return false;
        }
        *last = now;
        true
    }

    /// How many tokens are alive. For the tests below.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.live.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "054a5889ebc72ba5c7cc76f49ca6e0c18ea4237c";
    const OTHER: &str = "1053100314102abae6552ef0fddabe3df61fef07";

    #[test]
    fn a_token_opens_only_what_it_was_minted_for() {
        let t = StreamTokens::default();
        let token = t.issue(H, 0).expect("issue");

        assert!(t.verify(&token, H, 0), "its own file");
        assert!(!t.verify(&token, H, 1), "a different file of the same torrent");
        assert!(!t.verify(&token, OTHER, 0), "the same index of another torrent");
        assert!(!t.verify("not-a-token", H, 0), "something invented");
    }

    #[test]
    fn the_hash_is_compared_case_insensitively() {
        let t = StreamTokens::default();
        let token = t.issue(&H.to_ascii_uppercase(), 3).expect("issue");
        assert!(t.verify(&token, H, 3), "minted upper, used lower");
    }

    #[test]
    fn a_hash_that_is_not_an_info_hash_mints_nothing() {
        let t = StreamTokens::default();
        assert!(t.issue("", 0).is_none());
        assert!(t.issue("abc", 0).is_none(), "too short");
        assert!(t.issue(&"z".repeat(40), 0).is_none(), "not hex");
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn two_tokens_are_never_the_same() {
        let t = StreamTokens::default();
        let a = t.issue(H, 0).unwrap();
        let b = t.issue(H, 0).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "32 bytes as hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Both are good: minting does not revoke the previous one, or opening a
        // second tab would stop the first playing.
        assert!(t.verify(&a, H, 0) && t.verify(&b, H, 0));
    }

    /// The sweep is what stops an afternoon of clicking Play growing the map
    /// without bound. Checked through the public surface by making the entries
    /// look old rather than by sleeping for half an hour.
    #[test]
    fn expired_tokens_are_dropped_and_refused() {
        let t = StreamTokens::default();
        let stale = t.issue(H, 0).unwrap();
        {
            let mut live = t.live.lock().unwrap();
            let (_, last) = live.get_mut(&stale).unwrap();
            *last = Instant::now() - (TTL + Duration::from_secs(1));
        }
        assert!(!t.verify(&stale, H, 0), "past its life");
        assert_eq!(t.len(), 0, "and removed on the way out");

        // A fresh one issued afterwards sweeps whatever the failed verify left.
        let old = t.issue(H, 1).unwrap();
        {
            let mut live = t.live.lock().unwrap();
            let (_, last) = live.get_mut(&old).unwrap();
            *last = Instant::now() - (TTL + Duration::from_secs(1));
        }
        let fresh = t.issue(H, 2).unwrap();
        assert_eq!(t.len(), 1, "issuing swept the stale entry");
        assert!(t.verify(&fresh, H, 2));
    }

    #[test]
    fn using_a_token_extends_it() {
        let t = StreamTokens::default();
        let token = t.issue(H, 0).unwrap();
        // Almost dead...
        {
            let mut live = t.live.lock().unwrap();
            let (_, last) = live.get_mut(&token).unwrap();
            *last = Instant::now() - (TTL - Duration::from_secs(5));
        }
        assert!(t.verify(&token, H, 0), "still alive with 5s to go");
        // ...and that use should have reset the clock, so pushing it back by
        // almost the whole TTL again must still leave it valid.
        {
            let mut live = t.live.lock().unwrap();
            let (_, last) = live.get_mut(&token).unwrap();
            *last = Instant::now() - (TTL - Duration::from_secs(5));
        }
        assert!(t.verify(&token, H, 0), "the clock restarted on use");
    }

    #[test]
    fn a_wrong_scope_does_not_revoke_the_token() {
        let t = StreamTokens::default();
        let token = t.issue(H, 0).unwrap();
        assert!(!t.verify(&token, H, 9), "wrong file");
        assert!(t.verify(&token, H, 0), "but the token still works for its own");
    }
}
